use anyhow::anyhow;
use enumset::EnumSet;
use int_enum::IntEnum;

use crate::model::objects::ObjFlag;
use crate::model::permissions::Permissions;
use crate::model::props::{PropAttr, PropFlag};
use crate::model::var::Error::{
    E_INVARG, E_INVIND, E_PERM, E_PROPNF, E_RANGE, E_TYPE, E_VARNF, E_VERBNF,
};
use crate::model::var::{Error, Objid, Var};
use crate::model::verbs::{Program, VerbAttr};
use crate::model::ObjDB;
use crate::parsecmd::ParsedCommand;
use crate::vm::execute::FinallyReason::Fallthrough;
use crate::vm::opcode::{Binary, Op};
use crate::vm::state::{PersistentState, StateError};

/* Reasons for executing a FINALLY handler; constants are stored in DB, don't change order */
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, IntEnum)]
pub enum FinallyReason {
    Fallthrough = 0x00,
    Raise = 0x01,
    Uncatch = 0x02,
    Return = 0x03,
    Abort = 0x04,
    /* This doesn't actually get you into a FINALLY... */
    Exit = 0x065,
}

pub enum ExecutionOutcome {
  Done, // Task ran successfully to completion
    Aborted, // Task aborted, either by kill_task() or by an uncaught error.
    Blocked, // Task called a blocking built-in function.
}

struct Activation {
    binary: Binary,
    environment: Vec<Var>,
    valstack: Vec<Var>,
    pc: usize,
    error_pc: usize,
    temp: Var,
    this: Objid,
    player: Objid,
    player_flags: EnumSet<ObjFlag>,
    verb_owner: Objid,
    definer: Objid,
    verb: String,
}

impl Activation {
    pub fn new_for_method(
        binary: Binary,
        caller: Objid,
        this: Objid,
        player: Objid,
        player_flags: EnumSet<ObjFlag>,
        verb_owner: Objid,
        definer: Objid,
        verb: String,
        args: Vec<Var>,
    ) -> Result<Self, anyhow::Error> {
        let environment = vec![Var::None; binary.var_names.width()];

        let mut a = Activation {
            binary,
            environment,
            valstack: vec![],
            pc: 0,
            error_pc: 0,
            temp: Var::None,
            this,
            player,
            player_flags,
            verb_owner,
            definer,
            verb: verb.clone(),
        };

        a.set_var("this", Var::Obj(this)).unwrap();
        a.set_var("player", Var::Obj(player)).unwrap();
        a.set_var("caller", Var::Obj(caller)).unwrap();
        a.set_var("NUM", Var::Int(0)).unwrap();
        a.set_var("OBJ", Var::Int(1)).unwrap();
        a.set_var("STR", Var::Int(2)).unwrap();
        a.set_var("ERR", Var::Int(3)).unwrap();
        a.set_var("LIST", Var::Int(4)).unwrap();
        a.set_var("INT", Var::Int(0)).unwrap();
        a.set_var("FLOAT", Var::Int(9)).unwrap();

        a.set_var("verb", Var::Str(verb.clone())).unwrap();
        a.set_var("argstr", Var::Str(String::from(""))).unwrap();
        a.set_var("args", Var::List(args.clone())).unwrap();
        a.set_var("iobjstr", Var::Str(String::from(""))).unwrap();
        a.set_var("iobj", Var::Obj(Objid(-1))).unwrap();
        a.set_var("dobjstr", Var::Str(String::from(""))).unwrap();
        a.set_var("dobj", Var::Obj(Objid(-1))).unwrap();
        a.set_var("prepstr", Var::Str(String::from(""))).unwrap();

        Ok(a)
    }

    pub fn new_for_command(
        binary: Binary,
        caller: Objid,
        this: Objid,
        player: Objid,
        player_flags: EnumSet<ObjFlag>,
        verb_owner: Objid,
        definer: Objid,
        parsed_cmd: &ParsedCommand,
    ) -> Result<Self, anyhow::Error> {
        let environment = vec![Var::None; binary.var_names.width()];

        let mut a = Activation {
            binary,
            environment,
            valstack: vec![],
            pc: 0,
            error_pc: 0,
            temp: Var::None,
            this,
            player,
            player_flags,
            verb_owner,
            definer,
            verb: parsed_cmd.verb.clone(),
        };

        a.set_var("this", Var::Obj(this)).unwrap();
        a.set_var("player", Var::Obj(player)).unwrap();
        a.set_var("caller", Var::Obj(caller)).unwrap();
        a.set_var("NUM", Var::Int(0)).unwrap();
        a.set_var("OBJ", Var::Int(1)).unwrap();
        a.set_var("STR", Var::Int(2)).unwrap();
        a.set_var("ERR", Var::Int(3)).unwrap();
        a.set_var("LIST", Var::Int(4)).unwrap();
        a.set_var("INT", Var::Int(0)).unwrap();
        a.set_var("FLOAT", Var::Int(9)).unwrap();

        a.set_var("verb", Var::Str(parsed_cmd.verb.clone()))
            .unwrap();
        a.set_var("argstr", Var::Str(parsed_cmd.argstr.clone()))
            .unwrap();
        a.set_var("args", Var::List(parsed_cmd.args.clone()))
            .unwrap();
        a.set_var("iobjstr", Var::Str(parsed_cmd.iobjstr.clone()))
            .unwrap();
        a.set_var("iobj", Var::Obj(parsed_cmd.iobj)).unwrap();
        a.set_var("dobjstr", Var::Str(parsed_cmd.dobjstr.clone()))
            .unwrap();
        a.set_var("dobj", Var::Obj(parsed_cmd.dobj)).unwrap();
        a.set_var("prepstr", Var::Str(parsed_cmd.prepstr.clone()))
            .unwrap();

        Ok(a)
    }

    fn set_var(&mut self, name: &str, value: Var) -> Result<(), Error> {
        let n = self.binary.var_names.find_name_offset(name);
        if let Some(n) = n {
            self.environment[n] = value;
            Ok(())
        } else {
            Err(E_VARNF)
        }
    }

    pub fn next_op(&mut self) -> Option<Op> {
        if !self.pc < self.binary.main_vector.len() {
            return None;
        }
        let op = self.binary.main_vector[self.pc].clone();
        self.pc += 1;
        Some(op)
    }

    pub fn lookahead(&self) -> Option<Op> {
        self.binary.main_vector.get(self.pc + 1).cloned()
    }

    pub fn skip(&mut self) {
        self.pc += 1;
    }

    pub fn pop(&mut self) -> Option<Var> {
        self.valstack.pop()
    }

    pub fn push(&mut self, v: Var) {
        self.valstack.push(v)
    }

    pub fn peek_at(&self, i: usize) -> Option<Var> {
        if !i < self.valstack.len() {
            return None;
        }
        Some(self.valstack[i].clone())
    }

    pub fn peek(&self, width: usize) -> Vec<Var> {
        let l = self.valstack.len();
        Vec::from(&self.valstack[l - width..])
    }

    pub fn poke(&mut self, p: usize, v: &Var) {
        let l = self.valstack.len();
        self.valstack[l - p] = v.clone()
    }

    pub fn stack_size(&self) -> usize {
        self.valstack.len()
    }

    pub fn jump(&mut self, label_id: usize) {
        let label = &self.binary.jump_labels[label_id];
        self.pc += label.position;
    }

    pub fn rewind(&mut self, amt: usize) {
        self.pc -= amt;
    }
}

pub struct VM {
    // Activation stack.
    stack: Vec<Activation>,
}

macro_rules! binary_bool_op {
    ( $act:ident, $op:tt ) => {
        let rhs = $act.pop();
        let lhs = $act.pop();
        let result = if lhs $op rhs { 1 } else { 0 };
        $act.push(&Var::Int(result))
    };
}

macro_rules! binary_var_op {
    ( $act:ident, $op:tt ) => {
        let rhs = $act.pop();
        let lhs = $act.pop();
        let result = lhs.$op(&rhs);
        $act.push(&result)
    };
}

#[derive(Eq, PartialEq, Debug, Clone)]
pub enum ExecutionResult {
    Complete(Var),
    More,
}

impl VM {
    pub fn new() -> Self {
        Self { stack: vec![] }
    }
    pub fn raise_error(&mut self, _err: Error) {}

    fn top_mut(&mut self) -> &mut Activation {
        self.stack.last_mut().expect("activation stack underflow")
    }

    fn top(&self) -> &Activation {
        self.stack.last().expect("activation stack underflow")
    }

    fn pop(&mut self) -> Var {
        self.top_mut().pop().expect("stack underflow")
    }

    fn push(&mut self, v: &Var) {
        self.top_mut().push(v.clone())
    }

    fn next_op(&mut self) -> Option<Op> {
        self.top_mut().next_op()
    }

    fn jump(&mut self, label: usize) {
        self.top_mut().jump(label)
    }

    fn get_env(&mut self, id: usize) -> Var {
        self.top().environment[id].clone()
    }

    fn set_env(&mut self, id: usize, v: &Var) {
        self.top_mut().environment[id] = v.clone();
    }

    fn rewind(&mut self, amt: usize) {
        self.top_mut().rewind(amt);
    }

    fn peek(&self, amt: usize) -> Vec<Var> {
        self.top().peek(amt)
    }
    pub fn peek_at(&self, i: usize) -> Option<Var> {
        self.top().peek_at(i)
    }

    fn peek_top(&self) -> Var {
        self.top().peek(0)[0].clone()
    }

    fn poke(&mut self, pos: usize, v: &Var) {
        self.top_mut().poke(pos, v);
    }

    fn get_prop(
        &mut self,
        state: &dyn PersistentState,
        player_flags: EnumSet<ObjFlag>,
        propname: Var,
        obj: Var,
    ) -> Var {
        let Var::Str(propname) = propname else {
            return Var::Err(E_TYPE);
        };

        let Var::Obj(obj) = obj else {
            return Var::Err(E_INVIND);
        };

        match state.retrieve_property(obj, propname.as_str(), player_flags) {
            Ok(v) => v,
            Err(e) => match e.downcast_ref::<StateError>() {
                Some(StateError::PropertyPermissionDenied(_, _)) => Var::Err(E_PERM),
                Some(StateError::PropertyNotFound(_, _)) => Var::Err(E_PROPNF),
                _ => {
                    panic!("Unexpected error in property retrieval: {:?}", e);
                }
            },
        }
    }

    pub fn call_verb(
        &mut self,
        state: &mut impl PersistentState,
        this: Objid,
        verb: String,
        args: Vec<Var>,
        do_pass: bool,
    ) -> Result<ExecutionResult, anyhow::Error> {
        let this = if do_pass {
            if !state.valid(self.top().definer)? {
                self.push(&Var::Err(E_INVIND));
                return Ok(ExecutionResult::More);
            }
            state.parent_of(this)?
        } else {
            this
        };

        if !state.valid(this)? {
            self.push(&Var::Err(E_INVIND));
            return Ok(ExecutionResult::More);
        }
        // find callable verb
        let Ok((binary, verbinfo)) = state.retrieve_verb(this, verb.as_str()) else {
            self.push(&Var::Err(E_VERBNF));
            return Ok(ExecutionResult::More);
        };
        let top = self.top();
        let a = Activation::new_for_method(
            binary,
            top.definer,
            this,
            top.player,
            top.player_flags,
            verbinfo.attrs.owner.unwrap(),
            verbinfo.attrs.definer.unwrap(),
            verb,
            args,
        )?;

        self.stack.push(a);
        Ok(ExecutionResult::More)
    }

    pub fn do_method_verb(
        &mut self,
        state: &mut impl PersistentState,
        obj: Objid,
        verb_name: &str,
        do_pass: bool,
        this: Objid,
        player: Objid,
        player_flags: EnumSet<ObjFlag>,
        caller: Objid,
        args: Vec<Var>,
    ) -> Result<Var, anyhow::Error> {
        let (binary, vi) = match state.retrieve_verb(obj, verb_name) {
            Ok(binary) => binary,
            Err(e) => {
                return match e.downcast_ref::<StateError>() {
                    Some(StateError::VerbNotFound(_, _)) => Ok(Var::Err(E_VERBNF)),
                    Some(StateError::VerbPermissionDenied(_, _)) => Ok(Var::Err(E_PERM)),
                    _ => Err(e),
                };
            }
        };

        let a = Activation::new_for_method(
            binary,
            caller,
            this,
            player,
            player_flags,
            vi.attrs.owner.unwrap(),
            vi.attrs.definer.unwrap(),
            String::from(verb_name),
            args,
        )?;

        self.stack.push(a);

        Ok(Var::Err(Error::E_NONE))
    }

    pub fn exec(
        &mut self,
        state: &mut impl PersistentState,
    ) -> Result<ExecutionResult, anyhow::Error> {
        let op = self.next_op().expect("Unexpected program termination; opcode stream should end with RETURN or DONE");
        match op {
            Op::If(label) | Op::Eif(label) | Op::IfQues(label) | Op::While(label) => {
                let cond = self.pop();
                if cond.is_true() {
                    self.jump(label);
                }
            }
            Op::Jump { label } => {
                self.jump(label);
            }
            Op::WhileId { id, label } => {
                self.set_env(id, &self.peek_top());
                let cond = self.pop();
                if cond.is_true() {
                    self.jump(label);
                }
            }
            Op::ForList { label, id } => {
                let peek = self.peek(2);
                let (count, list) = (&peek[1], &peek[0]);
                let Var::Int(count) = count else {
                    self.raise_error(Error::E_TYPE);
                    self.pop();
                    self.pop();
                    self.jump(label);
                    return Ok(ExecutionResult::More);
                };
                let Var::List(l) = list else {
                    self.raise_error(Error::E_TYPE);
                    self.pop();
                    self.pop();
                    self.jump(label);
                    return Ok(ExecutionResult::More);
                };

                if *count as usize > l.len() {
                    self.pop();
                    self.pop();
                    self.jump(label);
                } else {
                    self.set_env(id, &l[*count as usize]);
                    self.poke(0, &Var::Int(*count + 1));
                    self.rewind(3);
                }
            }
            Op::ForRange { label, id } => {
                let peek = self.peek(2);
                let (to, from) = (&peek[1], &peek[0]);

                // TODO: LambdaMOO has special handling for MAXINT/MAXOBJ
                // Given we're 64-bit this is highly unlikely to ever be a concern for us, but
                // we also don't want to *crash* on obscene values, so impl that here.

                let next_val = match (to, from) {
                    (Var::Int(to_i), Var::Int(from_i)) => {
                        if to_i > from_i {
                            self.pop();
                            self.pop();
                            self.jump(label);
                            return Ok(ExecutionResult::More);
                        }
                        Var::Int(from_i + 1)
                    }
                    (Var::Obj(to_o), Var::Obj(from_o)) => {
                        if to_o.0 > from_o.0 {
                            self.pop();
                            self.pop();
                            self.jump(label);
                            return Ok(ExecutionResult::More);
                        }
                        Var::Obj(Objid(from_o.0 + 1))
                    }
                    (_, _) => {
                        self.raise_error(E_TYPE);
                        return Ok(ExecutionResult::More);
                    }
                };

                self.set_env(id, from);
                self.poke(1, &next_val);
                self.rewind(3);
            }
            Op::Pop => {
                self.pop();
            }
            Op::Val(val) => {
                self.push(&val);
            }
            Op::Imm(slot) => {
                // Peek ahead to see if the next operation is 'pop' and if so, just throw away.
                // MOO uses this to optimize verbdoc/comments, etc.
                match self.top().lookahead() {
                    Some(Op::Pop) => {
                        // skip
                        self.top_mut().skip();
                        return Ok(ExecutionResult::More);
                    }
                    _ => {}
                }
                let value = self.top().binary.literals[slot].clone();
                self.push(&value);
            }
            Op::MkEmptyList => self.push(&Var::List(vec![])),
            Op::ListAddTail => {
                let tail = self.pop();
                let list = self.pop();
                let Var::List(list) = list else {
                    self.push(&Var::Err(E_TYPE));
                    return Ok(ExecutionResult::More);
                };

                // TODO: quota check SVO_MAX_LIST_CONCAT -> E_QUOTA

                let mut new_list = list;
                new_list.push(tail);
                self.push(&Var::List(new_list))
            }
            Op::ListAppend => {
                let tail = self.pop();
                let list = self.pop();
                let Var::List(list) = list else {
                    self.push(&Var::Err(E_TYPE));
                    return Ok(ExecutionResult::More);
                };

                let Var::List(tail) = tail else {
                    self.push(&Var::Err(E_TYPE));
                    return Ok(ExecutionResult::More);
                };

                // TODO: quota check SVO_MAX_LIST_CONCAT -> E_QUOTA
                let new_list = list.into_iter().chain(tail.into_iter());
                self.push(&Var::List(new_list.collect()))
            }
            Op::IndexSet => {
                // collection[index] = value
                let value = self.pop(); /* rhs value */
                let index = self.pop(); /* index, must be int */
                let list = self.pop(); /* lhs except last index, should be list or str */

                let nval = match (list, index) {
                    (Var::List(l), Var::Int(i)) => {
                        if i < 0 || !i < l.len() as i64 {
                            self.push(&Var::Err(E_RANGE));
                            return Ok(ExecutionResult::More);
                        }

                        let mut nval = l;
                        nval[i as usize] = value;
                        Var::List(nval)
                    }
                    (Var::Str(s), Var::Int(i)) => {
                        if i < 0 || !i < s.len() as i64 {
                            self.push(&Var::Err(E_RANGE));
                            return Ok(ExecutionResult::More);
                        }

                        let Var::Str(value) = value else {
                            self.push(&Var::Err(E_INVARG));
                            return Ok(ExecutionResult::More);
                        };

                        if value.len() != 1 {
                            self.push(&Var::Err(E_INVARG));
                            return Ok(ExecutionResult::More);
                        }

                        let i = i as usize;
                        let (mut head, tail) = (String::from(&s[0..i]), &s[i + 1..]);
                        head.push_str(&value[0..1]);
                        head.push_str(tail);
                        Var::Str(head)
                    }
                    (_, _) => {
                        self.push(&Var::Err(E_TYPE));
                        return Ok(ExecutionResult::More);
                    }
                };
                self.push(&nval);
            }
            Op::MakeSingletonList => {
                let v = self.pop();
                self.push(&Var::List(vec![v]))
            }
            Op::CheckListForSplice => {}
            Op::PutTemp => {
                self.top_mut().temp = self.peek_top();
            }
            Op::PushTemp => {
                let tmp = self.top().temp.clone();
                self.push(&tmp);
                self.top_mut().temp = Var::None;
            }
            Op::Eq => {
                binary_bool_op!(self, ==);
            }
            Op::Ne => {
                binary_bool_op!(self, !=);
            }
            Op::Gt => {
                binary_bool_op!(self, >);
            }
            Op::Lt => {
                binary_bool_op!(self, <);
            }
            Op::Ge => {
                binary_bool_op!(self, >=);
            }
            Op::Le => {
                binary_bool_op!(self, <=);
            }
            Op::In => {
                let lhs = self.pop();
                let rhs = self.pop();
                self.push(&lhs.has_member(&rhs));
            }
            Op::Mul => {
                binary_var_op!(self, mul);
            }
            Op::Sub => {
                binary_var_op!(self, sub);
            }
            Op::Div => {
                binary_var_op!(self, div);
            }
            Op::Add => {
                binary_var_op!(self, add);
            }
            Op::Exp => {
                binary_var_op!(self, pow);
            }
            Op::Mod => {
                binary_var_op!(self, modulus);
            }
            Op::And(label) => {
                let v = self.pop().is_true();
                if !v {
                    self.jump(label)
                }
            }
            Op::Or(label) => {
                let v = self.pop().is_true();
                if v {
                    self.jump(label)
                }
            }
            Op::Not => {
                let v = !self.pop().is_true();
                self.push(&Var::Int(if v { 1 } else { 0 }));
            }
            Op::UnaryMinus => {
                let v = self.pop();
                self.push(&v.negative())
            }
            Op::Ref => {
                let index = self.pop();
                let l = self.pop();
                let Var::Int(index) = index else {
                    self.push(&Var::Err(E_TYPE));
                    return Ok(ExecutionResult::More);
                };
                self.push(&l.index(index as usize));
            }
            Op::Push(ident) => {
                let v = self.get_env(ident);
                match v {
                    Var::None => self.push(&Var::Err(E_VARNF)),
                    _ => self.push(&v),
                }
            }
            Op::Put(ident) => {
                let v = self.pop();
                self.set_env(ident, &v);
            }
            Op::PushRef => {
                let peek = self.peek(2);
                let (index, list) = (peek[1].clone(), peek[0].clone());
                let v = match (index, list) {
                    (Var::Int(index), Var::List(list)) => {
                        if index <= 0 || !index < list.len() as i64 {
                            Var::Err(E_RANGE)
                        } else {
                            list[index as usize].clone()
                        }
                    }
                    (_, _) => Var::Err(E_TYPE),
                };
                self.push(&v);
            }
            Op::RangeRef => {
                let (to, from, base) = (self.pop(), self.pop(), self.pop());
                let result = match (to, from, base) {
                    (Var::Int(to), Var::Int(from), Var::Str(base)) => {
                        if to < 0
                            || !to < base.len() as i64
                            || from < 0
                            || !from < base.len() as i64
                        {
                            Var::Err(E_RANGE)
                        } else {
                            let (from, to) = (from as usize, to as usize);
                            let substr = &base[from..to];
                            Var::Str(String::from(substr))
                        }
                    }
                    (Var::Int(to), Var::Int(from), Var::List(base)) => {
                        if to < 0
                            || !to < base.len() as i64
                            || from < 0
                            || !from < base.len() as i64
                        {
                            Var::Err(E_RANGE)
                        } else {
                            let (from, to) = (from as usize, to as usize);
                            let sublist = &base[from..to];
                            Var::List(Vec::from(sublist))
                        }
                    }
                    (_, _, _) => Var::Err(E_TYPE),
                };
                self.push(&result);
            }
            Op::GPut { id } => {
                self.set_env(id, &self.peek_top());
            }
            Op::GPush { id } => {
                let v = self.get_env(id);
                match v {
                    Var::None => self.push(&Var::Err(E_VARNF)),
                    _ => {
                        self.push(&v);
                    }
                }
            }
            Op::Length(offset) => {
                let v = self.peek_at(offset).unwrap();
                match v {
                    Var::Str(s) => self.push(&Var::Int(s.len() as i64)),
                    Var::List(l) => self.push(&Var::Int(l.len() as i64)),
                    _ => {
                        self.push(&Var::Err(E_TYPE));
                    }
                }
            }

            Op::Scatter {
                nargs, nreq, rest, ..
            } => {
                unimplemented!("scatter assignement");
            }

            Op::GetProp => {
                let (propname, obj) = (self.pop(), self.pop());
                let prop = self.get_prop(state, self.top().player_flags, propname, obj);
                self.push(&prop);
            }
            Op::PushGetProp => {
                let peeked = self.peek(2);
                let (propname, obj) = (peeked[0].clone(), peeked[1].clone());
                let pop = self.get_prop(state, self.top().player_flags, propname, obj);
                self.push(&pop);
            }
            Op::PutProp => {
                let (rhs, propname, obj) = (self.pop(), self.pop(), self.pop());
                let (propname, obj) = match (propname, obj) {
                    (Var::Str(propname), Var::Obj(obj)) => (propname, obj),
                    (_, _) => {
                        self.push(&Var::Err(E_TYPE));
                        return Ok(ExecutionResult::More);
                    }
                };
                match state.update_property(obj, &propname, self.top().player_flags, &rhs) {
                    Ok(()) => {
                        self.push(&Var::None);
                    }
                    Err(e) => match e.downcast_ref::<StateError>() {
                        _ => {
                            panic!("Unexpected error in property update: {:?}", e);
                        }
                        Some(StateError::PropertyNotFound(_, _)) => {
                            self.push(&Var::Err(E_PROPNF));
                        }
                        Some(StateError::PropertyPermissionDenied(_, _)) => {
                            self.push(&Var::Err(E_PERM));
                        }
                    },
                }
                return Ok(ExecutionResult::More);
            }
            Op::Fork { id: _, f_index: _ } => {
                unimplemented!("fork")
            }
            Op::CallVerb => {
                let (args, verb, obj) = (self.pop(), self.pop(), self.pop());
                let (args, verb, obj) = match (args, verb, obj) {
                    (Var::List(l), Var::Str(s), Var::Obj(o)) => (l, s, o),
                    (args, verb, obj) => {
                        self.push(&Var::Err(E_TYPE));
                        return Ok(ExecutionResult::More);
                    }
                };
                // TODO: check obj for validity, return E_INVIND if not

                return self.call_verb(state, obj, verb, args, false);
            }
            Op::Return => {
                let ret_val = self.pop();
                return self.unwind_stack(ret_val, FinallyReason::Return);
            }
            Op::Return0 => {
                return self.unwind_stack(Var::Int(0), FinallyReason::Return);
            }
            Op::Done => {
                let ret_val = Var::None;
                return self.unwind_stack(ret_val, FinallyReason::Return);
            }
            Op::FuncCall { id } => {
                // TODO Actually perform call. For now we just fake a return value.
                self.push(&Var::Err(E_PERM));
            }
            Op::PushLabel(label) => {
                self.push(&Var::Int(label as i64));
            }
            Op::TryFinally(label) => {
                self.push(&Var::_Finally(label));
            }
            Op::Catch => {
                self.push(&Var::_Catch(1));
            }
            Op::TryExcept(label) => {
                self.push(&Var::_Catch(label));
            }
            Op::EndCatch(label) => {
                let v = self.pop();
                let marker = self.pop();
                let Var::_Catch(marker) = marker else {
                    panic!("Stack marker is not type Catch");
                };
                for _i in 0..marker {
                    self.pop();
                }
                self.push(&v);
                self.jump(label);
            }
            Op::EndExcept(label) => {
                let marker = self.pop();
                let Var::_Catch(marker) = marker else {
                    panic!("Stack marker is not type Catch");
                };
                for _i in 0..marker {
                    self.pop();
                }
                self.jump(label);
            }
            Op::EndFinally => {
                let v = self.pop();
                let Var::_Finally(_marker) = v else {
                    panic!("Stack marker is not type Finally");
                };
                self.push(&Var::Int(Fallthrough.int_value() as i64));
                self.push(&Var::Int(0));
            }
            Op::Continue => {
                unimplemented!("continue")
            }
            Op::Exit(_label) => {
                unimplemented!("break")
            }
            _ => {
                panic!("Unexpected op: {:?} at PC: {}", op, self.top_mut().pc)
            }
        }
        return Ok(ExecutionResult::More);
    }

    fn unwind_stack(&mut self, value : Var, reason: FinallyReason) -> Result<ExecutionResult, anyhow::Error> {
        // TODO if errors raised, handle that all here. Unwind until we hit a finally block, etc.

        // Otherwise, there's two other paths: FinallyReason::Exit and FinallyReason::Return.
        // In the case of the latter, we pop the activation but immediately push 'val to the stack
        // of the new activation... unless it's the last, in which case execution
        // is complete.
        self.stack.pop().expect("Stack underflow");
        if self.stack.len() == 0 {
            return Ok(ExecutionResult::Complete(value));
        }
        self.push(&value);
        return Ok(ExecutionResult::More);
    }

}

#[cfg(test)]
mod tests {
    use crate::compiler::codegen::compile;
    use crate::compiler::parse::Names;
    use crate::model::objects::ObjFlag;
    use crate::model::r#match::{ArgSpec, PrepSpec, VerbArgsSpec};
    use crate::model::var::Error::{E_NONE, E_VERBNF};
    use crate::model::var::Var::Obj;
    use crate::model::var::{Objid, Var};
    use crate::model::verbs::{VerbAttrs, VerbFlag, VerbInfo, Vid};
    use crate::vm::execute::{ExecutionResult, VM};
    use crate::vm::opcode::Op::*;
    use crate::vm::opcode::{Binary, Op};
    use crate::vm::state::{PersistentState, StateError};
    use anyhow::Error;
    use enumset::EnumSet;
    use std::collections::HashMap;

    struct MockState {
        verbs: HashMap<(Objid, String), (Binary, VerbInfo)>,
        properties: HashMap<(Objid, String), Var>,
    }

    impl MockState {
        fn new() -> Self {
            Self {
                verbs: Default::default(),
                properties: Default::default(),
            }
        }
        fn set_verb(&mut self, o: Objid, name: &str, binary: &Binary) {
            self.verbs.insert(
                (o, name.to_string()),
                (
                    binary.clone(),
                    VerbInfo {
                        vid: Vid(0),
                        names: vec![name.to_string()],
                        attrs: VerbAttrs {
                            definer: Some(o),
                            owner: Some(o),
                            flags: Some(VerbFlag::Exec | VerbFlag::Read),
                            args_spec: Some(VerbArgsSpec {
                                dobj: ArgSpec::This,
                                prep: PrepSpec::None,
                                iobj: ArgSpec::This,
                            }),
                            program: None,
                        },
                    },
                ),
            );
        }

        fn compile_verb(&mut self, o: Objid, name: &str, code: &str) -> Binary {
            let binary = compile(code).unwrap();
            self.set_verb(o, name, &binary);
            binary
        }
    }

    fn mk_binary(main_vector: Vec<Op>, literals: Vec<Var>) -> Binary {
        let var_names = Names::new();

        Binary {
            literals,
            jump_labels: vec![],
            var_names,
            main_vector,
            fork_vectors: vec![],
        }
    }

    fn prepare_test_verb(
        verb_name: &str,
        vm: &mut VM,
        state: &mut MockState,
        opcodes: Vec<Op>,
        literals: Vec<Var>,
    ) {
        let o = Objid(0);
        state.set_verb(o, verb_name, &mk_binary(opcodes, literals));
    }

    fn call_verb(verb_name: &str, vm: &mut VM, state: &mut MockState) {
        let o = Objid(0);

        assert_eq!(
            vm.do_method_verb(
                state,
                o,
                verb_name,
                false,
                o,
                o,
                ObjFlag::Wizard | ObjFlag::Programmer,
                o,
                vec![],
            )
            .unwrap(),
            Var::Err(E_NONE),
        );
    }

    impl PersistentState for MockState {
        fn retrieve_verb(&self, obj: Objid, vname: &str) -> Result<(Binary, VerbInfo), Error> {
            let v = self.verbs.get(&(obj, vname.to_string()));
            match v {
                None => Err(StateError::VerbNotFound(obj, vname.to_string()).into()),
                Some(v) => Ok(v.clone()),
            }
        }

        fn retrieve_property(
            &self,
            obj: Objid,
            pname: &str,
            player_flags: EnumSet<ObjFlag>,
        ) -> Result<Var, Error> {
            let p = self.properties.get(&(obj, pname.to_string()));
            match p {
                None => Err(StateError::PropertyNotFound(obj, pname.to_string()).into()),
                Some(p) => Ok(p.clone()),
            }
        }

        fn update_property(
            &mut self,
            obj: Objid,
            pname: &str,
            player_flags: EnumSet<ObjFlag>,
            value: &Var,
        ) -> Result<(), Error> {
            self.properties
                .insert((obj, pname.to_string()), value.clone());
            Ok(())
        }

        fn parent_of(&mut self, obj: Objid) -> Result<Objid, Error> {
            Ok(Objid(-1))
        }

        fn valid(&mut self, obj: Objid) -> Result<bool, Error> {
            Ok(true)
        }
    }

    #[test]
    fn test_verbnf() {
        let mut vm = VM::new();
        let mut state = MockState::new();
        let o = Objid(0);
        assert_eq!(
            vm.do_method_verb(
                &mut state,
                o,
                "test",
                false,
                o,
                o,
                ObjFlag::Wizard | ObjFlag::Programmer,
                o,
                vec![],
            )
            .unwrap(),
            Var::Err(E_VERBNF)
        );
    }

    #[test]
    fn test_simple_vm_execute() {
        let mut vm = VM::new();
        let mut state = MockState::new();
        prepare_test_verb(
            "test",
            &mut vm,
            &mut state,
            vec![Imm(0), Pop, Done],
            vec![1.into()],
        );
        call_verb("test", &mut vm, &mut state);
        assert_eq!(vm.exec(&mut state).unwrap(), ExecutionResult::More);
        assert_eq!(vm.top().peek_at(0).unwrap(), Var::Int(1));
        assert_eq!(vm.exec(&mut state).unwrap(), ExecutionResult::More);
        assert_eq!(vm.top().stack_size(), 0);

        let ExecutionResult::Complete(result) = vm.exec(&mut state).unwrap() else {
            panic!("Expected Complete result");
        };
        assert_eq!(result, Var::None);
    }

    #[test]
    fn test_call_verb() {
        let mut vm = VM::new();
        let mut state = MockState::new();

        // Prepare two, chained, test verbs in our environment, with simple operations.

        // The first merely returns the value "666" immediately.
        prepare_test_verb(
            "test_return_verb",
            &mut vm,
            &mut state,
            vec![Imm(0), Return],
            vec![666.into()],
        );

        // The second actually calls the first verb, and returns the result.
        prepare_test_verb(
            "test_call_verb",
            &mut vm,
            &mut state,
            vec![Imm(0) /* obj */, Imm(1) /* verb */, Imm(2) /* args */, CallVerb, Return, Done],
            vec![Var::Obj(Objid(0)), Var::Str(String::from("test_return_verb")), Var::List(vec![])]
        );

        // Invoke the second verb
        call_verb("test_call_verb", &mut vm, &mut state);

        // Call repeatedly into exec until we ge either an error or Complete.
        let result = loop {
            match vm.exec(&mut state) {
                Ok(ExecutionResult::More) => continue,
                Ok(ExecutionResult::Complete(a)) => break a,
                Err(e) => panic!("error during execution: {:?}", e),
            }
        };

        assert_eq!(result, Var::Int(666));
    }
}
