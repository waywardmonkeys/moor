use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::compiler::builtins::BUILTINS;
use crate::compiler::labels::{Label, Offset};
use crate::db::state::WorldState;
use crate::model::ObjectError::{PropertyNotFound, PropertyPermissionDenied};
use crate::model::objects::ObjFlag;
use crate::model::var::{Error, ErrorPack, v_int, v_list, v_str, NOTHING, Objid, Var, v_bool};
use crate::model::var::Error::{
    E_ARGS, E_INVARG, E_INVIND, E_PERM, E_PROPNF, E_RANGE, E_TYPE, E_VARNF, E_VERBNF,
};
use crate::tasks::scheduler::TaskId;
use crate::tasks::Sessions;
use crate::util::bitenum::BitEnum;
use crate::vm::activation::{Activation, Caller};
use crate::vm::bf_server::BfNoop;
use crate::vm::opcode::{Op, ScatterLabel};

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum FinallyReason {
    Fallthrough,
    Raise {
        code: Error,
        msg: String,
        value: Var,
        stack: Vec<Var>,
    },
    Uncaught {
        code: Error,
        msg: String,
        value: Var,
        stack: Vec<Var>,
        backtrace: Vec<Var>,
    },
    Return(Var),
    Abort,
    Exit {
        stack: Offset,
        label: Label,
    },
}
const FINALLY_REASON_RAISE: usize = 0x00;
const FINALLY_REASON_UNCAUGHT: usize = 0x01;
const FINALLY_REASON_RETURN: usize = 0x02;
const FINALLY_REASON_ABORT: usize = 0x03;
const FINALLY_REASON_EXIT: usize = 0x04;
const FINALLY_REASON_FALLTHROUGH: usize = 0x05;

impl FinallyReason {
    pub fn code(&self) -> usize {
        match *self {
            FinallyReason::Fallthrough => FINALLY_REASON_RAISE,
            FinallyReason::Raise { .. } => FINALLY_REASON_RAISE,
            FinallyReason::Uncaught { .. } => FINALLY_REASON_UNCAUGHT,
            FinallyReason::Return(_) => FINALLY_REASON_RETURN,
            FinallyReason::Abort => FINALLY_REASON_ABORT,
            FinallyReason::Exit { .. } => FINALLY_REASON_EXIT,
        }
    }
    pub fn from_code(code: usize) -> FinallyReason {
        match code {
            FINALLY_REASON_RAISE => FinallyReason::Fallthrough,
            FINALLY_REASON_UNCAUGHT => FinallyReason::Fallthrough,
            FINALLY_REASON_RETURN => FinallyReason::Fallthrough,
            FINALLY_REASON_ABORT => FinallyReason::Fallthrough,
            FINALLY_REASON_EXIT => FinallyReason::Fallthrough,
            FINALLY_REASON_FALLTHROUGH => FinallyReason::Fallthrough,
            _ => panic!("Invalid FinallyReason code"),
        }
    }
}

pub enum ExecutionOutcome {
    Done,    // Task ran successfully to completion
    Aborted, // Task aborted, either by kill_task() or by an uncaught error.
    Blocked, // Task called a blocking built-in function.
}

#[async_trait]
pub(crate) trait BfFunction: Sync + Send {
    fn name(&self) -> &str;
    async fn call(
        &self,
        world_state: &mut dyn WorldState,
        frame: &mut Activation,
        sessions: Arc<RwLock<dyn Sessions>>,
        args: Vec<Var>,
    ) -> Result<Var, anyhow::Error>;
}

pub struct VM {
    // Activation stack.
    stack: Vec<Activation>,
    pub(crate) bf_funcs: Vec<Arc<Box<dyn BfFunction>>>,
}

#[derive(Eq, PartialEq, Debug, Clone)]
pub enum ExecutionResult {
    Complete(Var),
    More,
    Exception(FinallyReason),
}

macro_rules! binary_bool_op {
    ( $self:ident, $op:tt ) => {
        let rhs = $self.pop();
        let lhs = $self.pop();
        let result = if lhs $op rhs { 1 } else { 0 };
        $self.push(&Var::Int(result))
    };
}

macro_rules! binary_var_op {
    ( $self:ident, $op:tt ) => {
        let rhs = $self.pop();
        let lhs = $self.pop();
        let result = lhs.$op(&rhs);
        match result {
            Ok(result) => $self.push(&result),
            Err(err_code) => return $self.push_error(err_code),
        }
    };
}

impl Default for VM {
    fn default() -> Self {
        Self::new()
    }
}

impl VM {
    pub fn new() -> Self {
        let mut bf_funcs: Vec<Arc<Box<dyn BfFunction>>> = Vec::with_capacity(BUILTINS.len());
        for _ in 0..BUILTINS.len() {
            bf_funcs.push(Arc::new(Box::new(BfNoop {})))
        }
        let _bf_noop = Box::new(BfNoop {});
        let mut vm = Self {
            stack: vec![],
            bf_funcs,
        };

        vm.register_bf_server().unwrap();
        vm.register_bf_num().unwrap();
        vm.register_bf_values().unwrap();
        vm.register_bf_strings().unwrap();
        vm.register_bf_list_sets().unwrap();
        vm.register_bf_objects().unwrap();

        vm
    }

    fn find_handler_active(&mut self, raise_code: Error) -> Option<(usize, &Activation)> {
        // Scan activation frames and their stacks, looking for the first _Catch we can find.
        for a in self.stack.iter().rev() {
            let mut i = a.valstack.len();
            while i > 0 {
                if let Var::_Catch(cnt) = a.valstack[i - 1] {
                    // Found one, now scan forwards from 'cnt' backwards looking for either the first
                    // non-list value, or a list containing the error code.
                    // TODO check for 'cnt' being too large. not sure how to handle, tho
                    // TODO this actually i think is wrong, it needs to pull two values off the stack
                    for j in (i - cnt.0 as usize)..i {
                        if let Var::List(codes) = &a.valstack[j] {
                            if codes.contains(&Var::Err(raise_code)) {
                                return Some((i, a));
                            }
                        } else {
                            return Some((i, a));
                        }
                    }
                }
                i -= 1;
            }
        }
        None
    }

    fn make_stack_list(&self, frames: &[Activation], start_frame_num: usize) -> Vec<Var> {
        // TODO LambdaMOO had logic in here about 'root_vector' and 'line_numbers_too' that I haven't included yet.

        let mut stack_list = vec![];
        for (i, a) in frames.iter().rev().enumerate() {
            if i < start_frame_num {
                continue;
            }
            // Produce traceback line for each activation frame and append to stack_list
            // Should include line numbers (if possible), the name of the currently running verb,
            // its definer, its location, and the current player, and 'this'.
            let traceback_entry = vec![
                Var::Obj(a.this),
                Var::Str(a.verb_name().to_string()),
                Var::Obj(a.verb_definer()),
                Var::Obj(a.verb_owner()),
                Var::Obj(a.player),
                // TODO: find_line_number and add here.
            ];

            stack_list.push(Var::List(traceback_entry));
        }
        stack_list
    }

    fn error_backtrace_list(&self, raise_msg: &str) -> Vec<Var> {
        // Walk live activation frames and produce a written representation of a traceback for each
        // frame.
        let mut backtrace_list = vec![];
        for (i, a) in self.stack.iter().rev().enumerate() {
            let mut pieces = vec![];
            if i != 0 {
                pieces.push("... called from ".to_string());
            }
            pieces.push(format!("{}:{}", a.verb_definer(), a.verb_name()));
            if a.verb_definer() != a.this {
                pieces.push(format!(" (this == {})", a.this.0));
            }
            // TODO line number
            if i == 0 {
                pieces.push(format!(": {}", raise_msg));
            }
            // TODO builtin-function name if a builtin

            let piece = pieces.join("");
            backtrace_list.push(Var::Str(piece))
        }
        backtrace_list.push(v_str("(End of traceback)"));
        backtrace_list
    }

    fn raise_error_pack(&mut self, p: ErrorPack) -> Result<ExecutionResult, anyhow::Error> {
        // Look for first active catch handler's activation frame and its (reverse) offset in the activation stack.
        let handler_activ = self.find_handler_active(p.code);

        let why = if let Some((handler_active_num, _)) = handler_activ {
            FinallyReason::Raise {
                code: p.code,
                msg: p.msg,
                value: p.value,
                stack: self.make_stack_list(&self.stack, handler_active_num),
            }
        } else {
            FinallyReason::Uncaught {
                code: p.code,
                msg: p.msg.clone(),
                value: p.value,
                stack: self.make_stack_list(&self.stack, 0),
                backtrace: self.error_backtrace_list(p.msg.as_str()),
            }
        };

        self.unwind_stack(why)
    }

    fn push_error(&mut self, code: Error) -> Result<ExecutionResult, anyhow::Error> {
        self.push(&Var::Err(code));
        self.raise_error_pack(code.make_error_pack(None))
    }

    fn push_error_msg(
        &mut self,
        code: Error,
        msg: String,
    ) -> Result<ExecutionResult, anyhow::Error> {
        self.push(&Var::Err(code));
        self.raise_error_pack(code.make_error_pack(Some(msg)))
    }

    fn raise_error(&mut self, code: Error) -> Result<ExecutionResult, anyhow::Error> {
        self.raise_error_pack(code.make_error_pack(None))
    }

    fn unwind_stack(&mut self, why: FinallyReason) -> Result<ExecutionResult, anyhow::Error> {
        // Walk activation stack from bottom to top, tossing frames as we go.
        while let Some(a) = self.stack.last_mut() {
            // Pop the value stack seeking finally/catch handler values.
            while let Some(v) = a.valstack.pop() {
                match v {
                    Var::_Finally(label) => {
                        /* FINALLY handler */
                        let why_num = why.code();
                        if why_num == FinallyReason::Abort.code() {
                            continue;
                        }
                        a.jump(label);
                        a.push(Var::Int(why_num as i64));
                        return Ok(ExecutionResult::More);
                    }
                    Var::_Catch(_label) => {
                        /* TRY-EXCEPT or `expr ! ...' handler */
                        let FinallyReason::Raise{code, value, ..} = &why else {
                            continue
                        };
                        // Jump further back the value stack looking for an list of errors + labels
                        // we will match on.
                        let mut found = false;
                        if a.valstack.len() >= 2 {
                            if let (Some(Var::_Label(pushed_label)), Some(Var::List(error_codes))) =
                                (a.valstack.pop(), a.valstack.pop())
                            {
                                if error_codes.contains(&Var::Err(*code)) {
                                    a.jump(pushed_label);
                                    found = true;
                                }
                            }
                        }
                        if found {
                            a.push(value.clone());
                            return Ok(ExecutionResult::More);
                        }
                    }
                    _ => continue,
                }
            }
            if let FinallyReason::Exit { label, .. } = why {
                a.jump(label);
                return Ok(ExecutionResult::More);
            }

            // If we're doing a return, and this is the last activation, we're done and just pass
            // the returned value up out of the interpreter loop.
            // Otherwise pop off this activation, and continue unwinding.
            if let FinallyReason::Return(value) = &why {
                if self.stack.len() == 1 {
                    return Ok(ExecutionResult::Complete(value.clone()));
                }
            }

            if let FinallyReason::Uncaught {
                code: _,
                msg: _,
                value: _,
                stack: _,
                backtrace: _,
            } = &why
            {
                return Ok(ExecutionResult::Exception(why));
            }

            self.stack.pop().expect("Stack underflow");

            if self.stack.is_empty() {
                return Ok(ExecutionResult::Complete(Var::None));
            }
            // TODO builtin function unwinding stuff

            // If it was a return that brought us here, stick it onto the end of the next
            // activation's value stack.
            // (Unless we're the final activation, in which case that should have been handled
            // above)
            if let FinallyReason::Return(value) = why {
                self.push(&value);
                return Ok(ExecutionResult::More);
            }
        }

        // We realistically should not get here...
        panic!("Unwound stack to empty, but no exit condition was hit");
    }

    fn top_mut(&mut self) -> &mut Activation {
        self.stack.last_mut().expect("activation stack underflow")
    }

    fn top(&self) -> &Activation {
        self.stack.last().expect("activation stack underflow")
    }

    fn pop(&mut self) -> Var {
        self.top_mut()
            .pop()
            .unwrap_or_else(|| panic!("stack underflow, activation depth: {}", self.stack.len()))
    }

    fn push(&mut self, v: &Var) {
        self.top_mut().push(v.clone())
    }

    fn next_op(&mut self) -> Option<Op> {
        self.top_mut().next_op()
    }

    fn jump(&mut self, label: Label) {
        self.top_mut().jump(label)
    }

    fn get_env(&mut self, id: Label) -> Var {
        self.top().environment[id.0 as usize].clone()
    }

    fn set_env(&mut self, id: Label, v: &Var) {
        self.top_mut().environment[id.0 as usize] = v.clone();
    }

    fn peek(&self, amt: usize) -> Vec<Var> {
        self.top().peek(amt)
    }

    pub fn peek_at(&self, i: usize) -> Option<Var> {
        self.top().peek_at(i)
    }

    fn peek_top(&self) -> Var {
        self.top().peek_top().expect("stack underflow")
    }

    fn get_prop(
        &mut self,
        state: &mut dyn WorldState,
        player_flags: BitEnum<ObjFlag>,
        propname: Var,
        obj: Var,
    ) -> Result<ExecutionResult, anyhow::Error> {
        let Var::Str(propname) = propname else {
            return self.push_error(E_TYPE);
        };

        let Var::Obj(obj) = obj else {
            return self.push_error(E_INVIND);
        };

        let result = state.retrieve_property(obj, propname.as_str(), player_flags);
        let v = match result {
            Ok(v) => v,
            Err(e) => match e {
                PropertyPermissionDenied(_, _) => return self.push_error(E_PERM),
                PropertyNotFound(_, _) => return self.push_error(E_PROPNF),
                _ => {
                    panic!("Unexpected error in property retrieval: {:?}", e);
                }
            },
        };
        self.push(&v);
        Ok(ExecutionResult::More)
    }

    fn call_verb(
        &mut self,
        state: &mut dyn WorldState,
        this: Objid,
        verb: String,
        args: Vec<Var>,
        do_pass: bool,
    ) -> Result<ExecutionResult, anyhow::Error> {
        let this = if do_pass {
            let valid_definer = state.valid(self.top().verb_definer())?;

            if !valid_definer {
                return self.push_error(E_INVIND);
            }
            state.parent_of(this)?
        } else {
            this
        };

        let self_valid = state.valid(this)?;
        if !self_valid {
            return self.push_error(E_INVIND);
        }
        // find callable verb
        let result = state.retrieve_verb(this, verb.as_str());

        let Ok((binary, verbinfo)) = result else {
            return self.push_error_msg(E_VERBNF, format!("Verb \"{}\" not found", verb));
        };
        let top = self.top();
        let mut callers = top.callers.to_vec();
        let task_id = top.task_id;

        callers.push(Caller {
            this,
            verb_name: top.verb_name().to_string(),
            programmer: top.verb_owner(),
            verb_loc: top.verb_definer(),
            player: top.player,
            line_number: 0,
        });

        let a = Activation::new_for_method(
            task_id,
            binary,
            top.verb_definer(),
            this,
            top.player,
            top.player_flags,
            verbinfo,
            args,
            callers,
        )?;

        self.stack.push(a);
        Ok(ExecutionResult::More)
    }

    pub fn do_method_verb(
        &mut self,
        task_id: TaskId,
        state: &mut dyn WorldState,
        obj: Objid,
        verb_name: &str,
        _do_pass: bool,
        this: Objid,
        player: Objid,
        player_flags: BitEnum<ObjFlag>,
        _caller: Objid,
        args: Vec<Var>,
    ) -> Result<(), anyhow::Error> {
        let (binary, vi) = state.retrieve_verb(obj, verb_name)?;

        let a = Activation::new_for_method(
            task_id,
            binary,
            NOTHING,
            this,
            player,
            player_flags,
            vi,
            args,
            vec![],
        )?;

        self.stack.push(a);

        Ok(())
    }

    pub async fn exec(
        &mut self,
        state: &mut dyn WorldState,
        client_connection: Arc<RwLock<dyn Sessions>>,
    ) -> Result<ExecutionResult, anyhow::Error> {
        let op = self
            .next_op()
            .expect("Unexpected program termination; opcode stream should end with RETURN or DONE");

        match op {
            Op::If(label) | Op::Eif(label) | Op::IfQues(label) | Op::While(label) => {
                let cond = self.pop();
                if !cond.is_true() {
                    self.jump(label);
                }
            }
            Op::Jump { label } => {
                self.jump(label);
            }
            Op::WhileId { id, label } => {
                self.set_env(id, &self.peek_top());
                let cond = self.pop();
                if !cond.is_true() {
                    self.jump(label);
                }
            }
            Op::ForList { label, id } => {
                // Pop the count and list off the stack. We push back later when we re-enter.
                // TODO LambdaMOO had optimization here where it would only peek and update.
                // But I had some difficulty getting stack values right, so will do this simpler
                // for now and revisit later.
                let (count, list) = (&self.pop(), &self.pop());
                let Var::Int(count) = count else {
                    return self.raise_error(E_TYPE);

                    // LambdaMOO had a raise followed by jump. Not clear how that would work.
                    // Watch out for bugs here. Same below
                    // self.jump(label);
                };
                let count = *count as usize;
                let Var::List(l) = list else {
                    return self.raise_error(E_TYPE);
                    // self.jump(label);
                };

                // If we've exhausted the list, pop the count and list and jump out.
                if count >= l.len() {
                    self.jump(label);
                    return Ok(ExecutionResult::More);
                }

                // Track iteration count for range; set id to current list element for the count,
                // then increment the count, rewind the program counter to the top of the loop, and
                // continue.
                self.set_env(id, &l[count]);
                self.push(list);
                self.push(&Var::Int((count + 1) as i64));
            }
            Op::ForRange { label, id } => {
                // Pull the range ends off the stack.
                // TODO LambdaMOO had optimization here where it would only peek and update.
                // But I had some difficulty getting stack values right, so will do this simpler
                // for now and revisit later.
                let (to, from) = (&self.pop(), &self.pop());

                // TODO: LambdaMOO has special handling for MAXINT/MAXOBJ
                // Given we're 64-bit this is highly unlikely to ever be a concern for us, but
                // we also don't want to *crash* on obscene values, so impl that here.

                let next_val = match (to, from) {
                    (Var::Int(to_i), Var::Int(from_i)) => {
                        if from_i > to_i {
                            self.jump(label);
                            return Ok(ExecutionResult::More);
                        }
                        Var::Int(from_i + 1)
                    }
                    (Var::Obj(to_o), Var::Obj(from_o)) => {
                        if from_o.0 > to_o.0 {
                            self.jump(label);
                            return Ok(ExecutionResult::More);
                        }
                        Var::Obj(Objid(from_o.0 + 1))
                    }
                    (_, _) => {
                        return self.raise_error(E_TYPE);
                    }
                };

                self.set_env(id, from);
                self.push(&next_val);
                self.push(to);
            }
            Op::Pop => {
                self.pop();
            }
            Op::Val(val) => {
                self.push(&val);
            }
            Op::Imm(slot) => {
                // TODO Peek ahead to see if the next operation is 'pop' and if so, just throw away.
                // MOO uses this to optimize verbdoc/comments, etc.
                match self.top().lookahead() {
                    Some(Op::Pop) => {
                        // skip
                        self.top_mut().skip();
                        return Ok(ExecutionResult::More);
                    }
                    _ => {
                        let value = self.top().binary.literals[slot.0 as usize].clone();
                        self.push(&value);
                    }
                }
            }
            Op::MkEmptyList => self.push(&v_list(vec![])),
            Op::ListAddTail => {
                let tail = self.pop();
                let list = self.pop();
                let Var::List(list) = list else {
                    return self.push_error(E_TYPE);
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
                    return self.push_error(E_TYPE);
                };

                let Var::List(tail) = tail else {
                    return self.push_error(E_TYPE);
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
                            return self.push_error(E_RANGE);
                        }

                        let mut nval = l;
                        nval[i as usize] = value;
                        Var::List(nval)
                    }
                    (Var::Str(s), Var::Int(i)) => {
                        if i < 0 || !i < s.len() as i64 {
                            return self.push_error(E_RANGE);
                        }

                        let Var::Str(value) = value else {
                            return self.push_error(E_INVARG);
                        };

                        if value.len() != 1 {
                            return self.push_error(E_INVARG);
                        }

                        let i = i as usize;
                        let (mut head, tail) = (String::from(&s[0..i]), &s[i + 1..]);
                        head.push_str(&value[0..1]);
                        head.push_str(tail);
                        Var::Str(head)
                    }
                    (_, _) => {
                        return self.push_error(E_TYPE);
                    }
                };
                self.push(&nval);
            }
            Op::MakeSingletonList => {
                let v = self.pop();
                self.push(&v_list(vec![v]))
            }
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
                let r = lhs.has_member(&rhs);
                if let Var::Err(e) = r {
                    return self.push_error(e);
                }
                self.push(&r);
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
                let v = self.peek_top().is_true();
                if v {
                    self.jump(label);
                } else {
                    self.pop();
                }
            }
            Op::Not => {
                let v = !self.pop().is_true();
                self.push(&v_bool(v));
            }
            Op::UnaryMinus => {
                let v = self.pop();
                match v.negative() {
                    Err(e) => return self.push_error(e),
                    Ok(v) => self.push(&v),
                }
            }
            Op::Push(ident) => {
                let v = self.get_env(ident);
                match v {
                    Var::None => return self.push_error(E_VARNF),
                    _ => self.push(&v),
                }
            }
            Op::Put(ident) => {
                let v = self.peek_top();
                self.set_env(ident, &v);
            }
            Op::PushRef => {
                let peek = self.peek(2);
                let (index, list) = (peek[1].clone(), peek[0].clone());
                let v = match (index, list) {
                    (Var::Int(index), Var::List(list)) => {
                        if index <= 0 || !index < list.len() as i64 {
                            return self.push_error(E_RANGE);
                        } else {
                            list[index as usize].clone()
                        }
                    }
                    (_, _) => return self.push_error(E_TYPE),
                };
                self.push(&v);
            }
            Op::Ref => {
                let index = self.pop();
                let l = self.pop();
                let Var::Int(index) = index else {
                    return self.push_error(E_TYPE);
                };
                // MOO is 1-indexed.
                let index = (index - 1) as usize;
                match l.index(index) {
                    Err(e) => return self.push_error(e),
                    Ok(v) => self.push(&v),
                }
            }
            Op::RangeRef => {
                let (to, from, base) = (self.pop(), self.pop(), self.pop());
                match (to, from) {
                    (Var::Int(to), Var::Int(from)) => match base.range(from, to) {
                        Err(e) => return self.push_error(e),
                        Ok(v) => self.push(&v),
                    },
                    (_, _) => return self.push_error(E_TYPE),
                };
            }
            Op::RangeSet => {
                let (value, to, from, base) = (self.pop(), self.pop(), self.pop(), self.pop());
                match (to, from) {
                    (Var::Int(to), Var::Int(from)) => match base.rangeset(value, from, to) {
                        Err(e) => return self.push_error(e),
                        Ok(v) => self.push(&v),
                    },
                    _ => {
                        return self.push_error(E_TYPE);
                    }
                }
            }
            Op::GPut { id } => {
                self.set_env(id, &self.peek_top());
            }
            Op::GPush { id } => {
                let v = self.get_env(id);
                match v {
                    Var::None => return self.push_error(E_VARNF),
                    _ => {
                        self.push(&v);
                    }
                }
            }
            Op::Length(offset) => {
                let v = self.top().valstack[offset.0 as usize].clone();
                match v {
                    Var::Str(s) => self.push(&Var::Int(s.len() as i64)),
                    Var::List(l) => self.push(&Var::Int(l.len() as i64)),
                    _ => {
                        return self.push_error(E_TYPE);
                    }
                }
            }
            Op::GetProp => {
                let (propname, obj) = (self.pop(), self.pop());
                return self.get_prop(state, self.top().player_flags, propname, obj);
            }
            Op::PushGetProp => {
                let peeked = self.peek(2);
                let (propname, obj) = (peeked[0].clone(), peeked[1].clone());
                return self.get_prop(state, self.top().player_flags, propname, obj);
            }
            Op::PutProp => {
                let (rhs, propname, obj) = (self.pop(), self.pop(), self.pop());
                let (propname, obj) = match (propname, obj) {
                    (Var::Str(propname), Var::Obj(obj)) => (propname, obj),
                    (_, _) => {
                        return self.push_error(E_TYPE);
                    }
                };

                let update_result =
                    state.update_property(obj, &propname, self.top().player_flags, &rhs);

                match update_result {
                    Ok(()) => {
                        self.push(&Var::None);
                    }
                    Err(e) => match e {
                        PropertyNotFound(_, _) => {
                            return self.push_error(E_PROPNF);
                        }
                        PropertyPermissionDenied(_, _) => {
                            return self.push_error(E_PERM);
                        }
                        _ => {
                            panic!("Unexpected error in property update: {:?}", e);
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
                    _ => {
                        return self.push_error(E_TYPE);
                    }
                };
                // TODO: check obj for validity, return E_INVIND if not

                return self.call_verb(state, obj, verb, args, false);
            }
            Op::Return => {
                let ret_val = self.pop();
                return self.unwind_stack(FinallyReason::Return(ret_val));
            }
            Op::Return0 => {
                return self.unwind_stack(FinallyReason::Return(v_int(0)));
            }
            Op::Done => {
                return self.unwind_stack(FinallyReason::Return(Var::None));
            }
            Op::FuncCall { id } => {
                // Pop arguments, should be a list.
                let args = self.pop();
                let Var::List(args) = args else {
                    return self.push_error(E_ARGS);
                };
                if id.0 as usize >= self.bf_funcs.len() {
                    return self.push_error(E_VARNF);
                }
                let bf = self.bf_funcs[id.0 as usize].clone();
                let result = bf
                    .call(state, self.top_mut(), client_connection, args)
                    .await?;
                self.push(&result);
            }
            Op::PushLabel(label) => {
                self.push(&Var::_Label(label));
            }
            Op::TryFinally(label) => {
                self.push(&Var::_Finally(label));
            }
            Op::Catch => {
                self.push(&Var::_Catch(1.into()));
            }
            Op::TryExcept(label) => {
                self.push(&Var::_Catch(label));
            }
            Op::EndCatch(label) | Op::EndExcept(label) => {
                let is_catch = op == Op::EndCatch(label);
                let v = if is_catch { self.pop() } else { Var::None };
                let marker = self.pop();
                let Var::_Catch(marker) = marker else {
                    panic!("Stack marker is not type Catch");
                };
                for _i in 0..marker.0 {
                    self.pop(); /* handler PC */
                    self.pop(); /* code list */
                }
                if is_catch {
                    self.push(&v);
                }
                self.jump(label);
            }
            Op::EndFinally => {
                let v = self.pop();
                let Var::_Finally(_marker) = v else {
                    panic!("Stack marker is not type Finally");
                };
                self.push(&v_int(0) /* fallthrough */);
                self.push(&v_int(0));
            }
            Op::Continue => {
                let why = self.pop();
                let Var::Int(why) = why else {
                    panic!("'why' is not an integer representing a FinallyReason");
                };
                let why = FinallyReason::from_code(why as usize);
                match why {
                    FinallyReason::Fallthrough => {
                        // Do nothing, normal case.
                        return Ok(ExecutionResult::More);
                    }
                    FinallyReason::Raise { .. }
                    | FinallyReason::Uncaught { .. }
                    | FinallyReason::Return(_)
                    | FinallyReason::Exit { .. } => {
                        return self.unwind_stack(why);
                    }
                    FinallyReason::Abort => {
                        panic!("Unexpected FINALLY_ABORT in Continue")
                    }
                }
            }
            Op::ExitId(label) => {
                self.jump(label);
                return Ok(ExecutionResult::More);
            }
            Op::Exit { stack, label } => {
                return self.unwind_stack(FinallyReason::Exit { stack, label });
            }
            Op::Scatter {
                nargs,
                nreq,
                labels,
                done,
                ..
            } => {
                let list = self.peek_top();
                let Var::List(list) = list else {
                    self.pop();
                    return self.push_error(E_TYPE);
                };

                let len = list.len();
                if len < nreq.0 as usize {
                    self.pop();
                    return self.push_error(E_ARGS);
                }

                assert_eq!(nargs.0 as usize, labels.len());

                let mut jump_where = None;
                let mut args_iter = list.into_iter();
                for label in labels.iter() {
                    match label {
                        ScatterLabel::Required(id) => {
                            let Some(arg) = args_iter.next() else {
                                return self.push_error(E_ARGS);
                            };

                            self.set_env(*id, &arg);
                        }
                        ScatterLabel::Rest(id) => {
                            let mut v = vec![];
                            for _ in 1..nargs.0 {
                                v.push(args_iter.next().unwrap());
                            }
                            let rest = Var::List(v);
                            self.set_env(*id, &rest);
                        }
                        ScatterLabel::Optional(id, jump_to) => match args_iter.next() {
                            None => {
                                if jump_where.is_none() && jump_to.is_some() {
                                    jump_where = *jump_to;
                                }
                                break;
                            }
                            Some(v) => {
                                self.set_env(*id, &v);
                            }
                        },
                    }
                }
                match jump_where {
                    None => self.jump(done),
                    Some(jump_where) => self.jump(jump_where),
                }
            }
            Op::CheckListForSplice => {
                let Var::List(_) = self.peek_top() else {
                    self.pop();
                    return self.push_error(E_TYPE);
                };
            }
        }
        Ok(ExecutionResult::More)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use anyhow::Error;
    use async_trait::async_trait;
    use tokio::sync::RwLock;

    use crate::compiler::codegen::compile;
    use crate::compiler::labels::Names;
    use crate::db::CommitResult;
    use crate::db::state::WorldState;
    use crate::model::ObjectError;
    use crate::model::ObjectError::{PropertyNotFound, VerbNotFound};
    use crate::model::objects::ObjFlag;
    use crate::model::props::{PropAttrs, PropFlag};
    use crate::model::r#match::{ArgSpec, PrepSpec, VerbArgsSpec};
    use crate::model::var::{v_int, v_list, v_obj, v_str, Objid, Var};
    use crate::model::verbs::{VerbAttrs, VerbFlag, VerbInfo, Vid};
    use crate::tasks::parse_cmd::ParsedCommand;
    use crate::tasks::Sessions;
    use crate::util::bitenum::BitEnum;
    use crate::vm::execute::{ExecutionResult, VM};
    use crate::vm::opcode::{Binary, Op};
    use crate::vm::opcode::Op::*;

    struct NoopClientConnection {}
    impl NoopClientConnection {
        pub fn new() -> Self {
            Self {}
        }
    }

    #[async_trait]
    impl Sessions for NoopClientConnection {
        async fn send_text(&mut self, _player: Objid, _msg: String) -> Result<(), anyhow::Error> {
            Ok(())
        }

        async fn connected_players(&self) -> Result<Vec<Objid>, Error> {
            Ok(vec![])
        }
    }

    struct MockState {
        verbs: HashMap<(Objid, String), (Binary, VerbInfo)>,
        properties: HashMap<(Objid, String), Var>,
    }

    impl MockState {
        fn new() -> Box<Self> {
            let ws = Self {
                verbs: Default::default(),
                properties: Default::default(),
            };
            Box::new(ws)
        }

        pub fn new_with_verb(name: &str, binary: &Binary) -> Box<dyn WorldState> {
            let mut ws = Self {
                verbs: Default::default(),
                properties: Default::default(),
            };
            ws.set_verb(Objid(0), name, binary);
            Box::new(ws)
        }

        pub fn new_with_verbs(verbs: Vec<(&str, &Binary)>) -> Box<dyn WorldState> {
            let mut ws = Self {
                verbs: Default::default(),
                properties: Default::default(),
            };
            for (v, b) in verbs {
                ws.set_verb(Objid(0), v, b);
            }
            Box::new(ws)
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
                            flags: Some(BitEnum::new_with(VerbFlag::Exec) | VerbFlag::Read),
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
    }

    fn mk_binary(main_vector: Vec<Op>, literals: Vec<Var>, var_names: Names) -> Binary {
        Binary {
            literals,
            jump_labels: vec![],
            var_names,
            main_vector,
            fork_vectors: vec![],
        }
    }

    fn call_verb(state: &mut dyn WorldState, verb_name: &str, vm: &mut VM) {
        let o = Objid(0);

        assert!(vm
            .do_method_verb(
                0,
                state,
                o,
                verb_name,
                false,
                o,
                o,
                BitEnum::new_with(ObjFlag::Wizard) | ObjFlag::Programmer,
                o,
                vec![],
            )
            .is_ok());
    }

    impl WorldState for MockState {
        fn location_of(&mut self, _obj: Objid) -> Result<Objid, ObjectError> {
            unimplemented!()
        }

        fn contents_of(&mut self, _obj: Objid) -> Result<Vec<Objid>, ObjectError> {
            unimplemented!()
        }

        fn flags_of(&mut self, _obj: Objid) -> Result<BitEnum<ObjFlag>, ObjectError> {
            Ok(BitEnum::all())
        }

        fn verbs(&mut self, _obj: Objid) -> Result<Vec<VerbInfo>, ObjectError> {
            unimplemented!()
        }

        fn properties(&mut self, _obj: Objid) -> Result<Vec<(String, PropAttrs)>, ObjectError> {
            unimplemented!()
        }

        fn retrieve_verb(
            &mut self,
            obj: Objid,
            vname: &str,
        ) -> Result<(Binary, VerbInfo), ObjectError> {
            let v = self.verbs.get(&(obj, vname.to_string()));
            match v {
                None => Err(VerbNotFound(obj, vname.to_string())),
                Some(v) => Ok(v.clone()),
            }
        }

        fn retrieve_property(
            &mut self,
            obj: Objid,
            pname: &str,
            _player_flags: BitEnum<ObjFlag>,
        ) -> Result<Var, ObjectError> {
            let p = self.properties.get(&(obj, pname.to_string()));
            match p {
                None => Err(PropertyNotFound(obj, pname.to_string())),
                Some(p) => Ok(p.clone()),
            }
        }

        fn update_property(
            &mut self,
            obj: Objid,
            pname: &str,
            _player_flags: BitEnum<ObjFlag>,
            value: &Var,
        ) -> Result<(), ObjectError> {
            self.properties
                .insert((obj, pname.to_string()), value.clone());
            Ok(())
        }

        fn add_property(
            &mut self,
            obj: Objid,
            pname: &str,
            _owner: Objid,
            _prop_flags: BitEnum<PropFlag>,
            initial_value: Option<Var>,
        ) -> Result<(), ObjectError> {
            self.properties
                .insert((obj, pname.to_string()), initial_value.unwrap_or(Var::None));
            Ok(())
        }
        fn find_command_verb_on(
            &mut self,
            _oid: Objid,
            _pc: &ParsedCommand,
        ) -> Result<Option<VerbInfo>, ObjectError> {
            unimplemented!()
        }

        fn parent_of(&mut self, _obj: Objid) -> Result<Objid, ObjectError> {
            Ok(Objid(-1))
        }

        fn valid(&mut self, _obj: Objid) -> Result<bool, ObjectError> {
            Ok(true)
        }

        fn names_of(&mut self, _obj: Objid) -> Result<(String, Vec<String>), ObjectError> {
            unimplemented!()
        }

        fn commit(&mut self) -> Result<CommitResult, anyhow::Error> {
            Ok(CommitResult::Success)
        }

        fn rollback(&mut self) -> Result<(), anyhow::Error> {
            Ok(())
        }
    }

    fn exec_vm(state: &mut dyn WorldState, vm: &mut VM) -> Var {
        tokio_test::block_on(async {
            let client_connection = Arc::new(RwLock::new(NoopClientConnection::new()));
            // Call repeatedly into exec until we ge either an error or Complete.
            loop {
                match vm.exec(state, client_connection.clone()).await {
                    Ok(ExecutionResult::More) => continue,
                    Ok(ExecutionResult::Complete(a)) => return a,
                    Err(e) => panic!("error during execution: {:?}", e),
                    Ok(ExecutionResult::Exception(e)) => {
                        panic!("MOO exception {:?}", e);
                    }
                }
            }
        })
    }

    #[test]
    fn test_verbnf() {
        let mut state = MockState::new();
        let mut vm = VM::new();
        let o = Objid(0);

        match vm.do_method_verb(
            0,
            state.as_mut(),
            o,
            "test",
            false,
            o,
            o,
            BitEnum::new_with(ObjFlag::Wizard) | ObjFlag::Programmer,
            o,
            vec![],
        ) {
            Err(e) => match e.downcast::<ObjectError>() {
                Ok(VerbNotFound(vo, vs)) => {
                    assert_eq!(vo, o);
                    assert_eq!(vs, "test");
                }
                _ => {
                    panic!("expected verbnf error");
                }
            },
            _ => panic!("expected verbnf error"),
        }
    }

    #[test]
    fn test_simple_vm_execute() {
        let binary = mk_binary(vec![Imm(0.into()), Pop, Done], vec![1.into()], Names::new());
        let mut state = MockState::new_with_verb("test", &binary);
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, Var::None);
    }

    #[test]
    fn test_string_value_simple_indexing() {
        let mut state = MockState::new_with_verb(
            "test",
            &mk_binary(
                vec![Imm(0.into()), Imm(1.into()), Ref, Return, Done],
                vec![v_str("hello"), 2.into()],
                Names::new(),
            ),
        );
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_str("e"));
    }

    #[test]
    fn test_string_value_range_indexing() {
        let mut state = MockState::new_with_verb(
            "test",
            &mk_binary(
                vec![
                    Imm(0.into()),
                    Imm(1.into()),
                    Imm(2.into()),
                    RangeRef,
                    Return,
                    Done,
                ],
                vec![v_str("hello"), 2.into(), 4.into()],
                Names::new(),
            ),
        );
        let mut vm = VM::new();
        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_str("ell"));
    }

    #[test]
    fn test_list_value_simple_indexing() {
        let mut state = MockState::new_with_verb(
            "test",
            &mk_binary(
                vec![Imm(0.into()), Imm(1.into()), Ref, Return, Done],
                vec![
                    v_list(vec![111.into(), 222.into(), 333.into()]),
                    2.into(),
                ],
                Names::new(),
            ),
        );
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_int(222));
    }

    #[test]
    fn test_list_value_range_indexing() {
        let mut state = MockState::new_with_verb(
            "test",
            &mk_binary(
                vec![
                    Imm(0.into()),
                    Imm(1.into()),
                    Imm(2.into()),
                    RangeRef,
                    Return,
                    Done,
                ],
                vec![
                    v_list(vec![111.into(), 222.into(), 333.into()]),
                    2.into(),
                    3.into(),
                ],
                Names::new(),
            ),
        );
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_list(vec![222.into(), 333.into()]));
    }

    #[test]
    fn test_list_set_range() {
        let mut var_names = Names::new();
        let a = var_names.find_or_add_name("a");
        let mut state = MockState::new_with_verb(
            "test",
            &mk_binary(
                vec![
                    Imm(0.into()),
                    Put(a.0),
                    Pop,
                    Push(a.0),
                    Imm(1.into()),
                    Imm(2.into()),
                    Imm(3.into()),
                    PutTemp,
                    RangeSet,
                    Put(a.0),
                    Pop,
                    PushTemp,
                    Pop,
                    Push(a.0),
                    Return,
                    Done,
                ],
                vec![
                    v_list(vec![111.into(), 222.into(), 333.into()]),
                    2.into(),
                    3.into(),
                    v_list(vec![321.into(), 123.into()]),
                ],
                var_names,
            ),
        );
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, Var::List(vec![111.into(), 321.into(), 123.into()]));
    }

    #[test]
    fn test_list_splice() {
        let program = "a = {1,2,3,4,5}; return {@a[2..4]};";
        let binary = compile(program).unwrap();
        let mut state = MockState::new_with_verb("test", &binary);
        let mut vm = VM::new();
        let _args = binary.find_var("args");
        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_list(vec![2.into(), 3.into(), 4.into()]));
    }

    #[test]
    fn test_list_range_length() {
        let program = "return {{1,2,3}[2..$], {1}[$]};";
        let mut state = MockState::new_with_verb("test", &compile(program).unwrap());
        let mut vm = VM::new();
        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(
            result,
            v_list(vec![Var::List(vec![2.into(), 3.into()]), v_int(1)])
        );
    }

    #[test]
    fn test_if_or_expr() {
        let program = "if (1 || 0) return 1; else return 2; endif";
        let mut state = MockState::new_with_verb("test", &compile(program).unwrap());
        let mut vm = VM::new();
        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_int(1));
    }

    #[test]
    fn test_string_set_range() {
        let mut var_names = Names::new();
        let a = var_names.find_or_add_name("a");

        let mut state = MockState::new_with_verb(
            "test",
            &mk_binary(
                vec![
                    Imm(0.into()),
                    Put(a.0),
                    Pop,
                    Push(a.0),
                    Imm(1.into()),
                    Imm(2.into()),
                    Imm(3.into()),
                    PutTemp,
                    RangeSet,
                    Put(a.0),
                    Pop,
                    PushTemp,
                    Pop,
                    Push(a.0),
                    Return,
                    Done,
                ],
                vec![
                    v_str("mandalorian"),
                    4.into(),
                    7.into(),
                    v_str("bozo"),
                ],
                var_names,
            ),
        );
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_str("manbozorian"));
    }

    #[test]
    fn test_property_retrieval() {
        let mut state = MockState::new_with_verb(
            "test",
            &mk_binary(
                vec![Imm(0.into()), Imm(1.into()), GetProp, Return, Done],
                vec![v_obj(0), v_str("test_prop")],
                Names::new(),
            ),
        );
        {
            state
                .add_property(
                    Objid(0),
                    "test_prop",
                    Objid(0),
                    BitEnum::new_with(PropFlag::Read) | PropFlag::Write,
                    Some(v_int(666)),
                )
                .unwrap();
        }
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_int(666));
    }

    #[test]
    fn test_call_verb() {
        // Prepare two, chained, test verbs in our environment, with simple operations.

        // The first merely returns the value "666" immediately.
        let return_verb_binary = mk_binary(
            vec![Imm(0.into()), Return, Done],
            vec![v_int(666)],
            Names::new(),
        );

        // The second actually calls the first verb, and returns the result.
        let call_verb_binary = mk_binary(
            vec![
                Imm(0.into()), /* obj */
                Imm(1.into()), /* verb */
                Imm(2.into()), /* args */
                CallVerb,
                Return,
                Done,
            ],
            vec![
                v_obj(0),
                v_str("test_return_verb"),
                v_list(vec![]),
            ],
            Names::new(),
        );
        let mut state = MockState::new_with_verbs(vec![
            ("test_return_verb", &return_verb_binary),
            ("test_call_verb", &call_verb_binary),
        ]);
        let mut vm = VM::new();

        // Invoke the second verb
        call_verb(state.as_mut(), "test_call_verb", &mut vm);

        let result = exec_vm(state.as_mut(), &mut vm);

        assert_eq!(result, v_int(666));
    }

    #[test]
    fn test_assignment_from_range() {
        let program = "x = 1; y = {1,2,3}; x = x + y[2]; return x;";
        let binary = compile(program).unwrap();
        let mut state = MockState::new_with_verb("test", &binary);
        let mut vm = VM::new();
        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_int(3));
    }

    #[test]
    fn test_while_loop() {
        let program =
            "x = 0; while (x<100) x = x + 1; if (x == 75) break; endif endwhile return x;";
        let binary = compile(program).unwrap();
        let mut state = MockState::new_with_verb("test", &binary);
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_int(75));
    }

    #[test]
    fn test_while_labelled_loop() {
        let program = "x = 0; while broken (1) x = x + 1; if (x == 50) break; else continue broken; endif endwhile return x;";
        let binary = compile(program).unwrap();
        let mut state = MockState::new_with_verb("test", &binary);
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_int(50));
    }

    #[test]
    fn test_while_breaks() {
        let program = "x = 0; while (1) x = x + 1; if (x == 50) break; endif endwhile return x;";
        let binary = compile(program).unwrap();
        let mut state = MockState::new_with_verb("test", &binary);
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_int(50));
    }

    #[test]
    fn test_for_list_loop() {
        let program = "x = {1,2,3,4}; z = 0; for i in (x) z = z + i; endfor return {i,z};";
        let binary = compile(program).unwrap();
        let mut state = MockState::new_with_verb("test", &binary);
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_list(vec![v_int(4), v_int(10)]));
    }

    #[test]
    fn test_for_range_loop() {
        let program = "z = 0; for i in [1..4] z = z + i; endfor return {i,z};";
        let binary = compile(program).unwrap();
        let mut state = MockState::new_with_verb("test", &binary);
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_list(vec![v_int(4), v_int(10)]));
    }

    #[test]
    fn test_basic_scatter_assign() {
        let program = "{a, b, c, ?d = 4} = {1, 2, 3}; return {d, c, b, a};";
        let binary = compile(program).unwrap();
        let mut state = MockState::new_with_verb("test", &binary);
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(
            result,
            v_list(vec![v_int(4), v_int(3), v_int(2), v_int(1)])
        );
    }

    #[test]
    fn test_more_scatter_assign() {
        let program = "{a, b, @c} = {1, 2, 3, 4}; {x, @y, ?z} = {5,6,7,8}; return {a,b,c,x,y,z};";
        let binary = compile(program).unwrap();
        let mut state = MockState::new_with_verb("test", &binary);
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(
            result,
            Var::List(vec![
                v_int(1),
                v_int(2),
                v_list(vec![v_int(3), v_int(4)]),
                v_int(5),
                v_list(vec![v_int(6), v_int(7)]),
                v_int(8),
            ])
        );
    }

    #[test]
    fn test_conditional_expr() {
        let program = "return 1 ? 2 | 3;";
        let binary = compile(program).unwrap();
        let mut state = MockState::new_with_verb("test", &binary);
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_int(2));
    }

    #[test]
    fn test_catch_expr() {
        let program = "return {`x ! e_varnf => 666', `321 ! e_verbnf => 123'};";
        let binary = compile(program).unwrap();
        let mut state = MockState::new_with_verb("test", &binary);
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_list(vec![v_int(666), v_int(321)]));
    }

    #[test]
    fn test_try_except_stmt() {
        let program = "try a; except e (E_VARNF) return 666; endtry return 333;";
        let binary = compile(program).unwrap();
        let mut state = MockState::new_with_verb("test", &binary);
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_int(666));
    }

    #[test]
    fn test_try_finally_stmt() {
        let program = "try a; finally return 666; endtry return 333;";
        let binary = compile(program).unwrap();
        let mut state = MockState::new_with_verb("test", &binary);
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);
        let result = exec_vm(state.as_mut(), &mut vm);
        assert_eq!(result, v_int(666));
    }

    struct MockClientConnection {
        received: Vec<String>,
    }
    impl MockClientConnection {
        pub fn new() -> Self {
            Self { received: vec![] }
        }
    }
    #[async_trait]
    impl Sessions for MockClientConnection {
        async fn send_text(&mut self, _player: Objid, msg: String) -> Result<(), Error> {
            self.received.push(msg);
            Ok(())
        }

        async fn connected_players(&self) -> Result<Vec<Objid>, Error> {
            Ok(vec![])
        }
    }

    async fn exec_vm_with_mock_client_connection(
        vm: &mut VM,
        state: &mut dyn WorldState,
        client_connection: Arc<RwLock<MockClientConnection>>,
    ) -> Var {
        // Call repeatedly into exec until we ge either an error or Complete.
        loop {
            match vm.exec(state, client_connection.clone()).await {
                Ok(ExecutionResult::More) => continue,
                Ok(ExecutionResult::Complete(a)) => return a,
                Err(e) => panic!("error during execution: {:?}", e),
                Ok(ExecutionResult::Exception(e)) => {
                    panic!("MOO exception {:?}", e);
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_call_builtin() {
        let program = "return notify(#1, \"test\");";
        let binary = compile(program).unwrap();
        let mut state = MockState::new_with_verb("test", &binary);
        let mut vm = VM::new();

        call_verb(state.as_mut(), "test", &mut vm);

        let client_connection = Arc::new(RwLock::new(MockClientConnection::new()));
        let result =
            exec_vm_with_mock_client_connection(&mut vm, state.as_mut(), client_connection.clone())
                .await;
        assert_eq!(result, Var::None);

        assert_eq!(
            client_connection.read().await.received,
            vec!["test".to_string()]
        );
    }
}