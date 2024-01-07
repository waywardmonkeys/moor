// Copyright (C) 2024 Ryan Daum <ryan.daum@gmail.com>
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free Software
// Foundation, version 3.
//
// This program is distributed in the hope that it will be useful, but WITHOUT
// ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS
// FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License along with
// this program. If not, see <https://www.gnu.org/licenses/>.
//

// TODO: add fixed-size slotted page impl for Sized items, should be way more efficient for the
//       most common case of fixed-size tuples.
// TODO: implement the ability to expire and page-out tuples based on LRU or random/second
//       chance eviction (ala leanstore). will require separate PageIds from Bids, and will
//       involve rewriting SlotPtr on the fly to point to a new page when restored.
//       SlotPtr will also get a new field for last-access-time, so that we can do our eviction
// TODO: store indexes in here, too (custom paged datastructure impl)
// TODO: verify locking/concurrency safety of this thing -- loom test, stateright, or jepsen, etc.
// TODO: there is still some really gross stuff in here about the management of free space in
//       pages in the allocator list. It's probably causing excessive fragmentation because we're
//       considering only the reported available "content" area when fitting slots, and there seems
//       to be a sporadic failure where we end up with a "Page not found" error in the allocator on
//       free, meaning the page was not found in the used pages list.
//       whether any of this is worth futzing with after the fixed-size impl is done, I don't know.
// TODO: rename me, _I_ am the tuplebox. The "slots" are just where my tuples get stored. tho once
//       indexes are in here, things will get confusing (everything here assumes pages hold tuples)

use std::cmp::max;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::{Arc, Mutex};

use moor_values::util::{BitArray, Bitset64};
use thiserror::Error;
use tracing::{error, warn};

use crate::tuplebox::pool::{Bid, BufferPool, PagerError};
pub use crate::tuplebox::tuples::slotted_page::SlotId;
use crate::tuplebox::tuples::slotted_page::{
    slot_index_overhead, slot_page_empty_size, SlottedPage,
};
use crate::tuplebox::tuples::tuple_ptr::TuplePtr;
use crate::tuplebox::tuples::{TupleId, TupleRef};
use crate::tuplebox::RelationId;

pub type PageId = usize;

/// A SlotBox is a collection of (variable sized) pages, each of which is a collection of slots, each of which is holds
/// dynamically sized tuples.
pub struct SlotBox {
    inner: Mutex<Inner>,
}

#[derive(Debug, Clone, Error)]
pub enum SlotBoxError {
    #[error("Page is full, cannot insert slot of size {0} with {1} bytes remaining")]
    BoxFull(usize, usize),
    #[error("Tuple not found at index {0}")]
    TupleNotFound(usize),
}

impl SlotBox {
    pub fn new(virt_size: usize) -> Self {
        let pool = BufferPool::new(virt_size).expect("Could not create buffer pool");
        let inner = Mutex::new(Inner::new(pool));
        Self { inner }
    }

    /// Allocates a new slot for a tuple, somewhere in one of the pages we managed.
    /// Does not allow tuples from different relations to mix on the same page.
    pub fn allocate(
        self: Arc<Self>,
        size: usize,
        relation_id: RelationId,
        initial_value: Option<&[u8]>,
    ) -> Result<TupleRef, SlotBoxError> {
        let mut inner = self.inner.lock().unwrap();

        inner.do_alloc(size, relation_id, initial_value, &self)
    }

    pub(crate) fn load_page<LF: FnMut(Pin<&mut [u8]>)>(
        self: Arc<Self>,
        relation_id: RelationId,
        id: PageId,
        mut lf: LF,
    ) -> Result<Vec<TupleRef>, SlotBoxError> {
        let mut inner = self.inner.lock().unwrap();

        // Re-allocate the page.
        let page = inner.do_restore_page(id).unwrap();

        // Find all the slots referenced in this page.
        let slot_ids = page.load(|buf| {
            lf(buf);
        });

        // Now make sure we have swizrefs for all of them.
        let mut refs = vec![];
        for (slot, buflen, addr) in slot_ids.into_iter() {
            let tuple_id = TupleId { page: id, slot };
            let swizref = Box::pin(TuplePtr::create(self.clone(), tuple_id, addr, buflen));
            inner.swizrefs.insert(tuple_id, swizref);
            let swizref = inner.swizrefs.get_mut(&tuple_id).unwrap();
            let sp = unsafe { Pin::into_inner_unchecked(swizref.as_mut()) };
            let ptr = sp as *mut TuplePtr;
            let tuple_ref = TupleRef::at_ptr(ptr);
            refs.push(tuple_ref);
        }
        // The allocator needs to know that this page is used.
        inner.do_mark_page_used(relation_id, page.available_content_bytes(), id);
        Ok(refs)
    }

    #[inline(always)]
    pub(crate) fn page_for<'a>(&self, id: PageId) -> Result<SlottedPage<'a>, SlotBoxError> {
        let inner = self.inner.lock().unwrap();
        inner.page_for(id)
    }

    pub fn refcount(&self, id: TupleId) -> Result<u16, SlotBoxError> {
        let inner = self.inner.lock().unwrap();
        let page_handle = inner.page_for(id.page)?;
        page_handle.refcount(id.slot)
    }

    #[inline(always)]
    pub fn upcount(&self, id: TupleId) -> Result<(), SlotBoxError> {
        let inner = self.inner.lock().unwrap();
        let page_handle = inner.page_for(id.page)?;
        page_handle.upcount(id.slot)
    }

    #[inline(always)]
    pub fn dncount(&self, id: TupleId) -> Result<(), SlotBoxError> {
        let mut inner = self.inner.lock().unwrap();
        let page_handle = inner.page_for(id.page)?;
        if page_handle.dncount(id.slot)? {
            inner.do_remove(id)?;
        }
        Ok(())
    }

    #[inline(always)]
    pub fn get(&self, id: TupleId) -> Result<Pin<&[u8]>, SlotBoxError> {
        let inner = self.inner.lock().unwrap();
        let page_handle = inner.page_for(id.page)?;

        let lock = page_handle.read_lock();

        let slc = lock.get_slot(id.slot)?;
        Ok(slc)
    }

    pub fn update(
        self: Arc<Self>,
        relation_id: RelationId,
        id: TupleId,
        new_value: &[u8],
    ) -> Result<Option<TupleRef>, SlotBoxError> {
        let new_tup = {
            let mut inner = self.inner.lock().unwrap();
            let mut page_handle = inner.page_for(id.page)?;

            // If the value size is the same as the old value, we can just update in place, otherwise
            // it's a brand new allocation, and we have to remove the old one first.
            let mut page_write = page_handle.write_lock();
            let mut existing = page_write.get_slot_mut(id.slot).expect("Invalid tuple id");
            if existing.len() == new_value.len() {
                existing.copy_from_slice(new_value);
                return Ok(None);
            }
            inner.do_remove(id)?;

            inner.do_alloc(new_value.len(), relation_id, Some(new_value), &self)?
        };
        Ok(Some(new_tup))
    }

    pub fn update_with<F: FnMut(Pin<&mut [u8]>)>(
        &self,
        id: TupleId,
        mut f: F,
    ) -> Result<(), SlotBoxError> {
        let inner = self.inner.lock().unwrap();
        let mut page_handle = inner.page_for(id.page)?;
        let mut page_write = page_handle.write_lock();

        let existing = page_write.get_slot_mut(id.slot).expect("Invalid tuple id");

        f(existing);
        Ok(())
    }

    pub fn num_pages(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        inner.available_page_space.len()
    }

    pub fn used_pages(&self) -> Vec<PageId> {
        let allocator = self.inner.lock().unwrap();
        allocator
            .available_page_space
            .iter()
            .flat_map(|(_, ps)| ps.pages())
            .collect()
    }
}

struct Inner {
    // TODO: buffer pool has its own locks per size class, so we might not need this inside another lock
    //   *but* the other two items here are not thread-safe, and we need to maintain consistency across the three.
    //   so we can maybe get rid of the locks in the buffer pool...
    pool: BufferPool,
    /// The set of used pages, indexed by relation, in sorted order of the free space available in them.
    available_page_space: BitArray<PageSpace, 64, Bitset64<1>>,
    /// The "swizzelable" references to tuples, indexed by tuple id.
    /// There has to be a stable-memory address for each of these, as they are referenced by
    /// pointers in the TupleRefs themselves.
    // TODO: This needs to be broken down by page id, too, so that we can manage swap-in/swap-out at
    //   the page granularity.
    swizrefs: HashMap<TupleId, Pin<Box<TuplePtr>>>,
}

impl Inner {
    fn new(pool: BufferPool) -> Self {
        Self {
            available_page_space: BitArray::new(),
            pool,
            swizrefs: HashMap::new(),
        }
    }

    fn do_alloc(
        &mut self,
        size: usize,
        relation_id: RelationId,
        initial_value: Option<&[u8]>,
        sb: &Arc<SlotBox>,
    ) -> Result<TupleRef, SlotBoxError> {
        let tuple_size = size + slot_index_overhead();
        let page_size = max(32768, tuple_size.next_power_of_two());

        // Our selected page should not in theory get taken while we're holding this allocation lock,
        // but to be paranoid, we'll loop around and try again if it does.
        let mut tries = 0;
        loop {
            // Check if we have a free spot for this relation that can fit the tuple.
            let (page, offset) =
                { self.find_space(relation_id, tuple_size, slot_page_empty_size(page_size))? };
            let mut page_handle = self.page_for(page)?;
            let mut page_write_lock = page_handle.write_lock();
            if let Ok((slot, page_remaining, mut buf)) =
                page_write_lock.allocate(size, initial_value)
            {
                self.finish_alloc(page, relation_id, offset, page_remaining);

                // Make a swizzlable ptr reference and shove it in our set, and then return a tuple ref
                // which has a ptr to it.
                let buflen = buf.as_ref().len();
                let bufaddr = buf.as_mut_ptr();
                let tuple_id = TupleId { page, slot };

                // Heap allocate the swizref, and and pin it, take the address of it, then stick the swizref
                // in our set.
                let mut swizref = Box::pin(TuplePtr::create(sb.clone(), tuple_id, bufaddr, buflen));
                let swizaddr = unsafe { swizref.as_mut().get_unchecked_mut() } as *mut TuplePtr;
                self.swizrefs.insert(tuple_id, swizref);

                // Establish initial refcount using this existing lock.
                page_write_lock.upcount(slot).unwrap();

                return Ok(TupleRef::at_ptr(swizaddr));
            }
            tries += 1;
            if tries > 50 {
                panic!("Could not allocate tuple after {tries} tries");
            }
        }
    }

    fn do_restore_page<'a>(&mut self, id: PageId) -> Result<SlottedPage<'a>, SlotBoxError> {
        let (addr, page_size) = match self.pool.restore(Bid(id as u64)) {
            Ok(v) => v,
            Err(PagerError::CouldNotAccess) => {
                return Err(SlotBoxError::TupleNotFound(id));
            }
            Err(e) => {
                panic!("Unexpected buffer pool error: {:?}", e);
            }
        };

        Ok(SlottedPage::for_page(addr.load(SeqCst), page_size))
    }

    fn do_mark_page_used(&mut self, relation_id: RelationId, free_space: usize, pid: PageId) {
        let bid = Bid(pid as u64);
        let Some(available_page_space) = self.available_page_space.get_mut(relation_id.0) else {
            self.available_page_space
                .set(relation_id.0, PageSpace::new(free_space, bid));
            return;
        };

        available_page_space.insert(free_space, bid);
    }

    fn do_remove(&mut self, id: TupleId) -> Result<(), SlotBoxError> {
        let mut page_handle = self.page_for(id.page)?;
        let mut write_lock = page_handle.write_lock();

        let (new_free, _, is_empty) = write_lock.remove_slot(id.slot)?;
        self.report_free(id.page, new_free, is_empty);

        // TODO: The swizref stays just in case?
        // self.swizrefs.remove(&id);

        Ok(())
    }

    fn page_for<'a>(&self, page_num: usize) -> Result<SlottedPage<'a>, SlotBoxError> {
        let (page_address, page_size) = match self.pool.resolve_ptr::<u8>(Bid(page_num as u64)) {
            Ok(v) => v,
            Err(PagerError::CouldNotAccess) => {
                return Err(SlotBoxError::TupleNotFound(page_num));
            }
            Err(e) => {
                panic!("Unexpected buffer pool error: {:?}", e);
            }
        };
        let page_handle = SlottedPage::for_page(page_address, page_size);
        Ok(page_handle)
    }

    fn alloc(
        &mut self,
        relation_id: RelationId,
        page_size: usize,
    ) -> Result<(PageId, usize), SlotBoxError> {
        // Ask the buffer pool for a new page of the given size.
        let (bid, _, actual_size) = match self.pool.alloc(page_size) {
            Ok(v) => v,
            Err(PagerError::InsufficientRoom { desired, available }) => {
                return Err(SlotBoxError::BoxFull(desired, available));
            }
            Err(e) => {
                panic!("Unexpected buffer pool error: {:?}", e);
            }
        };
        match self.available_page_space.get_mut(relation_id.0) {
            Some(available_page_space) => {
                available_page_space.insert(slot_page_empty_size(actual_size), bid);
                Ok((bid.0 as PageId, available_page_space.len() - 1))
            }
            None => {
                self.available_page_space.set(
                    relation_id.0,
                    PageSpace::new(slot_page_empty_size(actual_size), bid),
                );
                Ok((bid.0 as PageId, 0))
            }
        }
    }

    /// Find room to allocate a new tuple of the given size, does not do the actual allocation yet,
    /// just finds the page to allocate it on.
    /// Returns the page id, and the offset into the `available_page_space` vector for that relation.
    fn find_space(
        &mut self,
        relation_id: RelationId,
        tuple_size: usize,
        page_size: usize,
    ) -> Result<(PageId, usize), SlotBoxError> {
        // Do we have a used pages set for this relation? If not, we can start one, and allocate a
        // new full page to it, and return. When we actually do the allocation, we'll be able to
        // find the page in the used pages set.
        let Some(available_page_space) = self.available_page_space.get_mut(relation_id.0) else {
            // Ask the buffer pool for a new buffer.
            return self.alloc(relation_id, page_size);
        };

        // Can we find some room?
        if let Some(found) = available_page_space.find_room(tuple_size) {
            return Ok(found);
        }

        // Out of room, need to allocate a new page.
        self.alloc(relation_id, page_size)
    }

    fn finish_alloc(
        &mut self,
        _pid: PageId,
        relation_id: RelationId,
        offset: usize,
        page_remaining_bytes: usize,
    ) {
        let available_page_space = self.available_page_space.get_mut(relation_id.0).unwrap();
        available_page_space.finish(offset, page_remaining_bytes);
    }

    fn report_free(&mut self, pid: PageId, new_size: usize, is_empty: bool) {
        for (_, available_page_space) in self.available_page_space.iter_mut() {
            if available_page_space.update_page(pid, new_size, is_empty) {
                if is_empty {
                    self.pool
                        .free(Bid(pid as u64))
                        .expect("Could not free page");
                }
                return;
            }
        }

        // TODO: initial textdump load seems to have a problem with initial inserts having a too-low refcount?
        //   but once the DB is established, it's fine. So maybe this is a problem with insert tuple allocation?
        warn!(
            "Page not found in used pages in allocator on free; pid {}; could be double-free, dangling weak reference?",
            pid
        );
    }
}

/// The amount of space available for each page known to the allocator for a relation.
/// Kept in two vectors, one for the available space, and one for the page ids, and kept sorted by
/// available space, with the page ids in the same order.
struct PageSpace {
    // Lower 64 bits of the page id, upper 64 bits are the size
    // In this way we can sort by available space, and keep the page ids in the same order
    // without a lot of gymnastics, and hopefully eventually use some SIMD instructions to do
    // the sorting?
    entries: Vec<u128>,
}

#[inline(always)]
fn decode(i: u128) -> (PageId, usize) {
    ((i & 0xFFFF_FFFF_FFFF) as PageId, (i >> 64) as usize)
}

#[inline(always)]
fn encode(pid: PageId, available: usize) -> u128 {
    (available as u128) << 64 | pid as u128
}

impl PageSpace {
    fn new(available: usize, bid: Bid) -> Self {
        Self {
            entries: vec![encode(bid.0 as PageId, available)],
        }
    }

    #[inline(always)]
    fn sort(&mut self) {
        self.entries.sort()
    }

    #[inline(always)]
    fn insert(&mut self, available: usize, bid: Bid) {
        self.entries.push(encode(bid.0 as PageId, available));
        self.sort();
    }

    #[inline(always)]
    fn seek(&self, pid: PageId) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| decode(*entry).0 == pid)
    }

    /// Update the allocation record for the page.
    fn update_page(&mut self, pid: PageId, available: usize, is_empty: bool) -> bool {
        // Page does not exist in this relation, so we can't update it.
        let Some(index) = self.seek(pid) else {
            return false;
        };

        // If the page is now totally empty, then we can remove it from the available_page_space vector.
        if is_empty {
            self.entries.remove(index);
        } else {
            // Otherwise, update the available space.
            let (pid, _) = decode(self.entries[index]);
            self.entries[index] = encode(pid, available);
        }
        self.sort();
        true
    }

    /// Find which page in this relation has room for a tuple of the given size.
    fn find_room(&self, available: usize) -> Option<(PageId, usize)> {
        // Look for the first page with enough space in our vector of used pages, which is kept
        // sorted by free space.
        let found = self
            .entries
            .binary_search_by(|entry| decode(*entry).1.cmp(&available));

        match found {
            // Exact match, highly unlikely, but possible.
            Ok(entry_num) => {
                // We only want the lower 64 bits, ala
                //
                let pid = (self.entries[entry_num] & 0xFFFF_FFFF_FFFFu128) as u64;
                Some((pid as PageId, entry_num))
            }
            // Out of room, our caller will need to allocate a new page.
            Err(position) if position == self.entries.len() => {
                // If we didn't find a page with enough space, then we need to allocate a new page.
                None
            }
            // Found a page we add to.
            Err(entry_num) => {
                let pid = self.entries[entry_num] as u64;
                Some((pid as PageId, entry_num))
            }
        }
    }

    fn finish(&mut self, offset: usize, page_remaining_bytes: usize) {
        let (pid, _) = decode(self.entries[offset]);
        self.entries[offset] = encode(pid, page_remaining_bytes);

        // If we (unlikely) consumed all the bytes, then we can remove the page from the avail pages
        // set.
        if page_remaining_bytes == 0 {
            self.entries.remove(offset);
        }
        self.sort();
    }

    fn pages(&self) -> impl Iterator<Item = PageId> + '_ {
        self.entries
            .iter()
            .map(|entry| (entry & 0xFFFF_FFFF_FFFF) as PageId)
    }

    #[inline(always)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rand::distributions::Alphanumeric;
    use rand::{thread_rng, Rng};

    use crate::tuplebox::tuples::slotbox::{SlotBox, SlotBoxError};
    use crate::tuplebox::tuples::slotted_page::slot_page_empty_size;
    use crate::tuplebox::tuples::TupleRef;
    use crate::tuplebox::RelationId;

    fn fill_until_full(sb: &Arc<SlotBox>) -> Vec<(TupleRef, Vec<u8>)> {
        let mut tuples = Vec::new();

        // fill until full... (SlotBoxError::BoxFull)
        loop {
            let mut rng = thread_rng();
            let tuple_len = rng.gen_range(1..(slot_page_empty_size(52000)));
            let value: Vec<u8> = rng.sample_iter(&Alphanumeric).take(tuple_len).collect();
            match TupleRef::allocate(RelationId(0), sb.clone(), 0, &value, &value) {
                Ok(tref) => {
                    tuples.push((tref, value));
                }
                Err(SlotBoxError::BoxFull(_, _)) => {
                    break;
                }
                Err(e) => {
                    panic!("Unexpected error: {:?}", e);
                }
            }
        }
        tuples
    }

    // Just allocate a single tuple, and verify that we can retrieve it.
    #[test]
    fn test_one_page_one_slot() {
        let sb = Arc::new(SlotBox::new(32768 * 64));
        let expected_value = vec![1, 2, 3, 4, 5];
        let _retrieved = sb
            .clone()
            .allocate(expected_value.len(), RelationId(0), Some(&expected_value))
            .unwrap();
    }

    // Fill just one page and verify that we can retrieve them all.
    #[test]
    fn test_one_page_a_few_slots() {
        let sb = Arc::new(SlotBox::new(32768 * 64));
        let mut tuples = Vec::new();
        let mut last_page_id = None;
        loop {
            let mut rng = thread_rng();
            let tuple_len = rng.gen_range(1..128);
            let tuple: Vec<u8> = rng.sample_iter(&Alphanumeric).take(tuple_len).collect();
            let tuple_id = sb
                .clone()
                .allocate(tuple.len(), RelationId(0), Some(&tuple))
                .unwrap();
            if let Some(last_page_id) = last_page_id {
                if last_page_id != tuple_id.id() {
                    break;
                }
            }
            last_page_id = Some(tuple_id.id());
            tuples.push((tuple_id, tuple));
        }
        for (tuple, expected_value) in tuples {
            let retrieved_buffer = tuple.slot_buffer();
            assert_eq!(
                expected_value,
                retrieved_buffer.as_slice(),
                "Slot value mismatch"
            );
        }
    }

    // Fill one page, then overflow into another, and verify we can get the tuple that's on the next page.
    #[test]
    fn test_page_overflow() {
        let sb = Arc::new(SlotBox::new(32768 * 64));
        let mut tuples = Vec::new();
        let mut first_page_id = None;
        let (next_page_tuple_id, next_page_value) = loop {
            let mut rng = thread_rng();
            let tuple_len = rng.gen_range(1..128);
            let tuple: Vec<u8> = rng.sample_iter(&Alphanumeric).take(tuple_len).collect();
            let tuple_id = sb
                .clone()
                .allocate(tuple.len(), RelationId(0), Some(&tuple))
                .unwrap();
            if let Some(last_page_id) = first_page_id {
                if last_page_id != tuple_id.id() {
                    break (tuple_id, tuple);
                }
            }
            first_page_id = Some(tuple_id.id());
            tuples.push((tuple_id, tuple));
        };
        for (tuple, expected_value) in tuples {
            let retrieved = tuple.slot_buffer();
            assert_eq!(expected_value, retrieved.as_slice());
        }
        // Now verify that the last tuple was on another, new page, and that we can retrieve it.
        assert_ne!(next_page_tuple_id.id(), first_page_id.unwrap());
        let retrieved = next_page_tuple_id.slot_buffer();
        assert_eq!(retrieved.as_slice(), next_page_value);
    }

    // Generate a pile of random sized tuples (which accumulate to more than a single page size),
    // and then scan back and verify their presence/equality.
    #[test]
    fn test_basic_add_fill_etc() {
        let sb = Arc::new(SlotBox::new(32768 * 32));
        let mut tuples = fill_until_full(&sb);
        for (i, (tuple, expected_value)) in tuples.iter().enumerate() {
            let retrieved_domain = tuple.domain();
            let retrieved_codomain = tuple.codomain();
            assert_eq!(
                *expected_value,
                retrieved_domain.as_slice(),
                "Mismatch at {}th tuple",
                i
            );
            assert_eq!(
                *expected_value,
                retrieved_codomain.as_slice(),
                "Mismatch at {}th tuple",
                i
            );
        }
        let used_pages = sb.used_pages();
        assert_ne!(used_pages.len(), tuples.len());

        // Now free all the tuples. This will destroy their refcounts.
        tuples.clear();
    }

    // Verify that filling our box up and then emptying it out again works. Should end up with
    // everything mmap DONTNEED'd, and we should be able to re-fill it again, too.
    #[test]
    fn test_full_fill_and_empty() {
        let sb = Arc::new(SlotBox::new(32768 * 64));
        let mut tuples = fill_until_full(&sb);

        // Collect the manual ids of the tuples we've allocated, so we can check them for refcount goodness.
        let ids = tuples.iter().map(|(t, _)| t.id()).collect::<Vec<_>>();
        tuples.clear();

        // Verify that everything is gone.
        for id in ids {
            assert!(sb.get(id).is_err());
        }
    }

    // Fill a box with tuples, then go and free some random ones, verify their non-presence, then
    // fill back up again and verify the new presence.
    #[test]
    fn test_fill_and_free_and_refill_etc() {
        let sb = Arc::new(SlotBox::new(32768 * 64));
        let mut tuples = fill_until_full(&sb);
        let mut rng = thread_rng();
        let mut freed_tuples = Vec::new();

        // Pick a bunch of tuples at random to free, and remove them from the tuples set, which should dncount
        // them to 0, freeing them.
        let to_remove = tuples.len() / 2;
        for _ in 0..to_remove {
            let idx = rng.gen_range(0..tuples.len());
            let (tuple, value) = tuples.remove(idx);
            let id = tuple.id();
            freed_tuples.push((id, value));
        }

        // What we expected to still be there is there.
        for (tuple, expected_value) in &tuples {
            let retrieved_domain = tuple.domain();
            let retrieved_codomain = tuple.codomain();
            assert_eq!(*expected_value, retrieved_domain.as_slice());
            assert_eq!(*expected_value, retrieved_codomain.as_slice());
        }
        // What we expected to not be there is not there.
        for (id, _) in freed_tuples {
            assert!(sb.get(id).is_err());
        }
        // Now fill back up again.
        let new_tuples = fill_until_full(&sb);
        // Verify both the new tuples and the old tuples are there.
        for (tuple, expected) in new_tuples {
            let retrieved_domain = tuple.domain();
            let retrieved_codomain = tuple.codomain();
            assert_eq!(expected, retrieved_domain.as_slice());
            assert_eq!(expected, retrieved_codomain.as_slice());
        }
        for (tuple, expected) in tuples {
            let retrieved_domain = tuple.domain();
            let retrieved_codomain = tuple.codomain();
            assert_eq!(expected, retrieved_domain.as_slice());
            assert_eq!(expected, retrieved_codomain.as_slice());
        }
    }

    #[test]
    fn alloc_encode_decode() {
        let pid = 12345;
        let available = 54321;
        let encoded = super::encode(pid, available);
        let (decoded_pid, decoded_available) = super::decode(encoded);
        assert_eq!(pid, decoded_pid);
        assert_eq!(available, decoded_available);
    }
}
