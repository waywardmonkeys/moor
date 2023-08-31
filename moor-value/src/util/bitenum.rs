use binary_layout::LayoutAs;
use std::marker::PhantomData;
use std::ops::{BitOr, BitOrAssign};

use bincode::{Decode, Encode};
/// A barebones minimal custom bitset enum, to replace use of `EnumSet` crate which was not rkyv'able.
use num_traits::ToPrimitive;

#[derive(Debug, Clone, Copy, Ord, PartialOrd, Eq, PartialEq, Hash, Encode, Decode)]
pub struct BitEnum<T: ToPrimitive> {
    value: u16,
    phantom: PhantomData<T>,
}

impl<T: ToPrimitive> LayoutAs<u16> for BitEnum<T> {
    fn read(v: u16) -> Self {
        Self {
            value: v,
            phantom: PhantomData,
        }
    }

    fn write(v: Self) -> u16 {
        v.to_u16()
    }
}

impl<T: ToPrimitive> BitEnum<T> {
    #[must_use] pub fn new() -> Self {
        Self {
            value: 0,
            phantom: PhantomData,
        }
    }
    #[must_use] pub fn to_u16(&self) -> u16 {
        self.value
    }

    #[must_use] pub fn from_u8(value: u8) -> Self {
        Self {
            value: u16::from(value),
            phantom: PhantomData,
        }
    }

    pub fn new_with(value: T) -> Self {
        let mut s = Self {
            value: 0,
            phantom: PhantomData,
        };
        s.set(value);
        s
    }

    #[must_use] pub fn all() -> Self {
        Self {
            value: u16::MAX,
            phantom: PhantomData,
        }
    }

    pub fn set(&mut self, value: T) {
        self.value |= 1 << value.to_u64().unwrap();
    }

    pub fn clear(&mut self, value: T) {
        self.value &= !(1 << value.to_u64().unwrap());
    }

    pub fn contains(&self, value: T) -> bool {
        self.value & (1 << value.to_u64().unwrap()) != 0
    }
}

impl<T: ToPrimitive> BitOr for BitEnum<T> {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self {
            value: self.value | rhs.value,
            phantom: PhantomData,
        }
    }
}

impl<T: ToPrimitive> Default for BitEnum<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: ToPrimitive> BitOrAssign<T> for BitEnum<T> {
    fn bitor_assign(&mut self, rhs: T) {
        self.set(rhs);
    }
}

impl<T: ToPrimitive> BitOr<T> for BitEnum<T> {
    type Output = Self;

    fn bitor(self, rhs: T) -> Self::Output {
        let mut s = self;
        s.set(rhs);
        s
    }
}

impl<T: ToPrimitive> From<T> for BitEnum<T> {
    fn from(value: T) -> Self {
        Self::new_with(value)
    }
}
