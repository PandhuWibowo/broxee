//! Promises, the microtask queue, `async` functions that pause at `await`, and generators
//! that pause at `yield`.
//!
//! The evaluator recurses, so it cannot stop in the middle of a call. An `async` body is run
//! instead by a small statement machine with its own stack of frames (blocks, loops, `try`,
//! `switch`), which can stop between statements and resume later. Before a statement runs,
//! the `await`s it evaluates unconditionally are settled one at a time (pausing for each);
//! their results wait in `Interp::slots`, keyed by the `await` node's address, and the
//! statement then runs with the ordinary evaluator, which takes them from there. An `await`
//! in a conditional position (`a && await b`, a ternary branch) is not prepared: it gives the
//! value of a settled promise, and throws for a pending one.
//!
//! A statement without `await` runs whole with the ordinary evaluator, so only the path to an
//! `await` pays for the machine.

use std::collections::HashMap;
use std::sync::Arc;

use super::{Ctrl, EnvId, Interp, Kind, ObjId, Value};
use crate::parser::{Body, Elem, Expr, Func, Name, Prop, Stmt, VarKind};
use crate::Host;

/// Jobs run per microtask checkpoint before the page's scripts are stopped.
pub const MICROTASK_LIMIT: usize = 200_000;
/// `async` calls waiting at an `await` at once.
const PAUSED_LIMIT: usize = 20_000;

pub enum PromiseState {
    Pending(Vec<Reaction>),
    Fulfilled(Value),
    Rejected(Value),
}

pub struct PromiseData {
    pub state: PromiseState,
    /// Something reacts to it: a rejection is not reported as uncaught.
    pub handled: bool,
}

#[derive(Clone)]
pub enum Handler {
    /// Settle the result promise the same way.
    Pass,
    Call(Value),
    /// `finally`: call it, then settle the result the same way as the original.
    Finally(Value),
    /// Resume paused `async` call `n`.
    Resume(usize),
    /// Element `index` of aggregate `n` (`Promise.all` and friends) settled.
    Element(usize, usize),
}

pub struct Reaction {
    on_ok: Handler,
    on_err: Handler,
    result: Option<ObjId>,
}

pub enum Job {
    React {
        handler: Handler,
        value: Value,
        ok: bool,
        result: Option<ObjId>,
    },
    /// Resolve `promise` with `thenable` by calling its `then`.
    Thenable {
        promise: ObjId,
        thenable: Value,
        then: Value,
    },
    /// `queueMicrotask`.
    Call(Value),
}

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    All,
    AllSettled,
    Any,
}

pub struct Aggregate {
    result: ObjId,
    values: Vec<Value>,
    remaining: usize,
    mode: Mode,
}

/// A place in `Interp::coros`.
pub enum Slot {
    Free,
    /// Taken out while it runs: a call it makes must not get this place.
    Running,
    Paused(Coro),
}

/// A paused (or running) `async` or generator call.
pub struct Coro {
    frames: Vec<Frame>,
    /// What an `async` call settles (a generator has none).
    promise: Option<ObjId>,
    slots: HashMap<usize, Result<Value, Value>>,
    /// The values a `yield*` is handing out, and the next one.
    delegate: Option<(Vec<Value>, usize)>,
}

/// How a generator is resumed.
pub enum Resume {
    Next(Value),
    Throw(Value),
    Return(Value),
}

/// Statements a frame walks: a list, or one statement (a branch or loop body).
#[derive(Clone)]
enum Stmts {
    Many(Body),
    One(Arc<Stmt>),
}

impl Stmts {
    fn len(&self) -> usize {
        match self {
            Stmts::Many(b) => b.len(),
            Stmts::One(_) => 1,
        }
    }

    fn get(&self, i: usize) -> &Stmt {
        match self {
            Stmts::Many(b) => &b[i],
            Stmts::One(s) => s,
        }
    }
}

/// A statement held across a pause.
#[derive(Clone)]
struct At {
    list: Stmts,
    i: usize,
}

impl At {
    fn stmt(&self) -> &Stmt {
        self.list.get(self.i)
    }
}

/// What a `Prepare` frame does once its `await`s are settled.
#[derive(Clone, Copy, PartialEq)]
enum Then {
    /// Run the statement.
    Run,
    /// Start the compound statement, its head expression ready.
    Start,
    /// The loop frame below evaluates its test.
    Test,
}

enum Phase {
    /// Evaluate the test (or take the next item) and run the body.
    Next,
    /// The body finished or continued.
    AfterBody,
}

enum LoopKind {
    While,
    DoWhile,
    For { per_pass: bool, outer: EnvId },
    Items(super::Source),
}

enum TryPhase {
    Body,
    Catch,
    Finally,
}

enum Frame {
    /// Statements run in order. `label`: `break label` ends it (a labeled statement).
    Seq {
        list: Stmts,
        next: usize,
        env: EnvId,
        label: Option<Name>,
    },
    /// Settling the `await`s of a statement (or its head) one by one.
    Prepare {
        at: At,
        env: EnvId,
        then: Then,
        next: usize,
    },
    Loop {
        at: At,
        env: EnvId,
        label: Option<Name>,
        kind: LoopKind,
        phase: Phase,
    },
    Switch {
        at: At,
        env: EnvId,
        case: usize,
        next: usize,
    },
    Try {
        at: At,
        env: EnvId,
        phase: TryPhase,
        saved: Option<Completion>,
    },
}

#[derive(Debug)]
enum Completion {
    Normal,
    Return(Value),
    Break(Option<Name>),
    Continue(Option<Name>),
    Throw(Value),
}

fn completion(r: Result<(), Ctrl>) -> Result<Completion, Ctrl> {
    match r {
        Ok(()) => Ok(Completion::Normal),
        Err(Ctrl::Return(v)) => Ok(Completion::Return(v)),
        Err(Ctrl::Break(l)) => Ok(Completion::Break(l)),
        Err(Ctrl::Continue(l)) => Ok(Completion::Continue(l)),
        Err(Ctrl::Throw(v)) => Ok(Completion::Throw(v)),
        Err(abort) => Err(abort),
    }
}

/// What one step of the machine did.
enum Step {
    Done(Completion),
    Pushed,
    Pause(ObjId),
    Yield(Value),
}

fn key(e: &Expr) -> usize {
    e as *const Expr as usize
}

/// The `await`s `e` evaluates whatever happens, innermost first.
fn awaits<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    let elems = |items: &'a [Elem], out: &mut Vec<&'a Expr>| {
        for item in items {
            if let Elem::Item(e) | Elem::Spread(e) = item {
                awaits(e, out);
            }
        }
    };
    match e {
        Expr::Await(inner) | Expr::Yield(Some(inner), _) => {
            awaits(inner, out);
            out.push(e);
        }
        Expr::Yield(None, _) => out.push(e),
        Expr::Template(_, parts) | Expr::Seq(parts) => parts.iter().for_each(|p| awaits(p, out)),
        Expr::Array(items) => elems(items, out),
        Expr::Object(props) => {
            for p in props {
                match p {
                    Prop::Init(k, v) => {
                        awaits(k, out);
                        awaits(v, out);
                    }
                    Prop::Spread(v) | Prop::Get(v, _) | Prop::Set(v, _) => awaits(v, out),
                }
            }
        }
        Expr::Unary(_, a)
        | Expr::Update(_, _, a)
        | Expr::SuperMember(a)
        | Expr::Destructure(_, a) => awaits(a, out),
        Expr::Binary(_, a, b) | Expr::Assign(_, a, b) | Expr::Member(a, b, _) => {
            awaits(a, out);
            awaits(b, out);
        }
        // Only the first operand and the test always run.
        Expr::Logical(_, a, _) | Expr::Cond(a, _, _) => awaits(a, out),
        Expr::Call(f, args, _) | Expr::New(f, args) => {
            awaits(f, out);
            elems(args, out);
        }
        Expr::SuperCall(args) => elems(args, out),
        _ => {}
    }
}

/// Whether `e` holds an `await` anywhere outside nested functions.
fn has_await(e: &Expr) -> bool {
    let elems = |items: &[Elem]| {
        items
            .iter()
            .any(|i| matches!(i, Elem::Item(e) | Elem::Spread(e) if has_await(e)))
    };
    match e {
        Expr::Await(_) | Expr::Yield(..) => true,
        Expr::Template(_, parts) | Expr::Seq(parts) => parts.iter().any(has_await),
        Expr::Array(items) => elems(items),
        Expr::Object(props) => props.iter().any(|p| match p {
            Prop::Init(k, v) => has_await(k) || has_await(v),
            Prop::Spread(v) | Prop::Get(v, _) | Prop::Set(v, _) => has_await(v),
        }),
        Expr::Unary(_, a)
        | Expr::Update(_, _, a)
        | Expr::SuperMember(a)
        | Expr::Destructure(_, a) => has_await(a),
        Expr::Binary(_, a, b)
        | Expr::Assign(_, a, b)
        | Expr::Member(a, b, _)
        | Expr::Logical(_, a, b) => has_await(a) || has_await(b),
        Expr::Cond(a, b, c) => has_await(a) || has_await(b) || has_await(c),
        Expr::Call(f, args, _) | Expr::New(f, args) => has_await(f) || elems(args),
        Expr::SuperCall(args) => elems(args),
        _ => false,
    }
}

/// Whether running `s` can reach an `await` (nested functions aside).
fn suspends(s: &Stmt) -> bool {
    let list = |b: &[Stmt]| b.iter().any(suspends);
    match s {
        Stmt::Expr(e) | Stmt::Throw(e) => has_await(e),
        Stmt::Var(_, decls) => decls.iter().any(|(_, e)| e.as_ref().is_some_and(has_await)),
        Stmt::Return(e) => e.as_ref().is_some_and(has_await),
        Stmt::If(t, a, b) => has_await(t) || suspends(a) || b.as_deref().is_some_and(suspends),
        Stmt::Block(b) => list(b),
        Stmt::While(t, b) | Stmt::DoWhile(b, t) => has_await(t) || suspends(b),
        Stmt::For(init, t, u, b) => {
            init.as_deref().is_some_and(suspends)
                || t.as_ref().is_some_and(has_await)
                || u.as_ref().is_some_and(has_await)
                || suspends(b)
        }
        Stmt::ForIn(_, _, o, b, _) => has_await(o) || suspends(b),
        Stmt::Try(b, h, f) => {
            list(b) || h.as_ref().is_some_and(|(_, h)| list(h)) || f.as_deref().is_some_and(list)
        }
        Stmt::Switch(v, cases) => {
            has_await(v)
                || cases
                    .iter()
                    .any(|(t, b)| t.as_ref().is_some_and(has_await) || list(b))
        }
        Stmt::Labeled(_, b) => suspends(b),
        _ => false,
    }
}

/// The `await`s to settle before `then` for statement `s`.
fn prepared(s: &Stmt, then: Then) -> Vec<&Expr> {
    let mut out = Vec::new();
    match (s, then) {
        (Stmt::Expr(e) | Stmt::Throw(e) | Stmt::Return(Some(e)), Then::Run) => awaits(e, &mut out),
        (Stmt::Var(_, decls), Then::Run) => {
            for (_, e) in decls {
                if let Some(e) = e {
                    awaits(e, &mut out);
                }
            }
        }
        (Stmt::If(e, ..) | Stmt::ForIn(_, _, e, ..) | Stmt::Switch(e, _), Then::Start) => {
            awaits(e, &mut out)
        }
        (Stmt::While(e, _) | Stmt::DoWhile(_, e), Then::Test) => awaits(e, &mut out),
        (Stmt::For(_, Some(e), _, _), Then::Test) => awaits(e, &mut out),
        _ => {}
    }
    out
}

impl Interp {
    // ---- Promises ----

    pub fn new_promise(&mut self) -> Result<ObjId, Ctrl> {
        let proto = self.protos.promise;
        self.alloc(
            Some(proto),
            Kind::Promise(Box::new(PromiseData {
                state: PromiseState::Pending(Vec::new()),
                handled: false,
            })),
        )
    }

    fn promise_data(&mut self, p: ObjId) -> Option<&mut PromiseData> {
        match &mut self.objects[p as usize].kind {
            Kind::Promise(d) => Some(d),
            _ => None,
        }
    }

    pub fn is_promise(&self, v: &Value) -> Option<ObjId> {
        match v {
            Value::Obj(o) if matches!(self.objects[*o as usize].kind, Kind::Promise(_)) => Some(*o),
            _ => None,
        }
    }

    /// Fulfill (`ok`) or reject `p` with `v`, once.
    pub fn settle(&mut self, p: ObjId, ok: bool, v: Value) {
        let Some(data) = self.promise_data(p) else {
            return;
        };
        let PromiseState::Pending(reactions) = &mut data.state else {
            return;
        };
        let reactions = std::mem::take(reactions);
        data.state = if ok {
            PromiseState::Fulfilled(v.clone())
        } else {
            PromiseState::Rejected(v.clone())
        };
        if !ok && !data.handled {
            self.unhandled.push(p);
        }
        for r in reactions {
            self.microtasks.push_back(Job::React {
                handler: if ok { r.on_ok } else { r.on_err },
                value: v.clone(),
                ok,
                result: r.result,
            });
        }
    }

    /// Resolve `p` with `v`: adopt it when it is a promise or has a `then`.
    pub fn resolve_promise(&mut self, host: &mut dyn Host, p: ObjId, v: Value) -> Result<(), Ctrl> {
        match self.promise_data(p) {
            Some(PromiseData {
                state: PromiseState::Pending(_),
                ..
            }) => {}
            _ => return Ok(()),
        }
        if let Some(q) = self.is_promise(&v) {
            if q == p {
                let e = self.make_error("TypeError", "a promise cannot resolve to itself")?;
                self.settle(p, false, e);
                return Ok(());
            }
            self.subscribe(q, Handler::Pass, Handler::Pass, Some(p));
            return Ok(());
        }
        if let Value::Obj(_) = v {
            match self.get(host, &v, "then") {
                Ok(Some(then)) if self.is_callable(&then) => {
                    self.microtasks.push_back(Job::Thenable {
                        promise: p,
                        thenable: v,
                        then,
                    });
                    return Ok(());
                }
                Err(Ctrl::Throw(e)) => {
                    self.settle(p, false, e);
                    return Ok(());
                }
                Err(abort @ Ctrl::Abort(_)) => return Err(abort),
                _ => {}
            }
        }
        self.settle(p, true, v);
        Ok(())
    }

    /// React to `q` settling.
    pub fn subscribe(&mut self, q: ObjId, on_ok: Handler, on_err: Handler, result: Option<ObjId>) {
        let Some(data) = self.promise_data(q) else {
            return;
        };
        data.handled = true;
        let job = match &mut data.state {
            PromiseState::Pending(list) => {
                list.push(Reaction {
                    on_ok,
                    on_err,
                    result,
                });
                return;
            }
            PromiseState::Fulfilled(v) => Job::React {
                handler: on_ok,
                value: v.clone(),
                ok: true,
                result,
            },
            PromiseState::Rejected(v) => Job::React {
                handler: on_err,
                value: v.clone(),
                ok: false,
                result,
            },
        };
        self.microtasks.push_back(job);
    }

    /// `promise.then(on_ok, on_err)`: a new promise for what they return.
    pub fn promise_then(&mut self, q: ObjId, on_ok: Value, on_err: Value) -> Result<ObjId, Ctrl> {
        let result = self.new_promise()?;
        let handler = |it: &Interp, f: Value| {
            if it.is_callable(&f) {
                Handler::Call(f)
            } else {
                Handler::Pass
            }
        };
        let (ok, err) = (handler(self, on_ok), handler(self, on_err));
        self.subscribe(q, ok, err, Some(result));
        Ok(result)
    }

    /// `Promise.resolve(v)`: `v` itself when it is a promise.
    pub fn promise_resolved(&mut self, host: &mut dyn Host, v: Value) -> Result<ObjId, Ctrl> {
        if let Some(p) = self.is_promise(&v) {
            return Ok(p);
        }
        let p = self.new_promise()?;
        self.resolve_promise(host, p, v)?;
        Ok(p)
    }

    /// `Promise.all`, `allSettled` and `any` over `items`.
    pub fn promise_aggregate(
        &mut self,
        host: &mut dyn Host,
        items: Vec<Value>,
        mode: Mode,
    ) -> Result<ObjId, Ctrl> {
        let result = self.new_promise()?;
        if items.is_empty() {
            if mode == Mode::Any {
                let e = self.make_error("AggregateError", "all promises were rejected")?;
                self.settle(result, false, e);
            } else {
                let empty = self.new_array(Vec::new())?;
                self.settle(result, true, empty);
            }
            return Ok(result);
        }
        let n = self.aggregates.len();
        self.aggregates.push(Aggregate {
            result,
            values: vec![Value::Undefined; items.len()],
            remaining: items.len(),
            mode,
        });
        for (i, item) in items.into_iter().enumerate() {
            let p = self.promise_resolved(host, item)?;
            self.subscribe(p, Handler::Element(n, i), Handler::Element(n, i), None);
        }
        Ok(result)
    }

    /// The resolve and reject functions handed to an executor or a thenable.
    pub fn resolvers(&mut self, p: ObjId) -> Result<(Value, Value), Ctrl> {
        let f = self.protos.function;
        let ok = self.alloc(Some(f), Kind::Resolver(p, false))?;
        let err = self.alloc(Some(f), Kind::Resolver(p, true))?;
        Ok((Value::Obj(ok), Value::Obj(err)))
    }

    /// `queueMicrotask(f)`.
    pub fn queue_microtask(&mut self, f: Value) {
        self.microtasks.push_back(Job::Call(f));
    }

    /// Run queued jobs until none are left, as browsers do after each task.
    pub fn run_microtasks(&mut self, host: &mut dyn Host) -> Result<(), Ctrl> {
        let mut ran = 0;
        while let Some(job) = self.microtasks.pop_front() {
            ran += 1;
            if ran > MICROTASK_LIMIT {
                self.microtasks.clear();
                return Err(Ctrl::Abort("too many promise reactions in one task".into()));
            }
            self.run_job(host, job)?;
        }
        self.aggregates.clear();
        Ok(())
    }

    fn run_job(&mut self, host: &mut dyn Host, job: Job) -> Result<(), Ctrl> {
        match job {
            Job::Call(f) => match self.call(host, &f, Value::Undefined, &[]) {
                Err(abort @ Ctrl::Abort(_)) => Err(abort),
                Err(Ctrl::Throw(e)) => {
                    let line = format!("Uncaught {}", self.describe(host, &e));
                    host.log(&line);
                    Ok(())
                }
                _ => Ok(()),
            },
            Job::Thenable {
                promise,
                thenable,
                then,
            } => {
                let (ok, err) = self.resolvers(promise)?;
                match self.call(host, &then, thenable, &[ok, err]) {
                    Err(Ctrl::Throw(e)) => self.settle(promise, false, e),
                    Err(abort @ Ctrl::Abort(_)) => return Err(abort),
                    _ => {}
                }
                Ok(())
            }
            Job::React {
                handler,
                value,
                ok,
                result,
            } => self.react(host, handler, value, ok, result),
        }
    }

    fn react(
        &mut self,
        host: &mut dyn Host,
        handler: Handler,
        value: Value,
        ok: bool,
        result: Option<ObjId>,
    ) -> Result<(), Ctrl> {
        let pass = |it: &mut Interp, host: &mut dyn Host, ok: bool, v: Value| match result {
            Some(r) if ok => it.resolve_promise(host, r, v),
            Some(r) => {
                it.settle(r, false, v);
                Ok(())
            }
            None => Ok(()),
        };
        match handler {
            Handler::Pass => pass(self, host, ok, value),
            Handler::Call(f) => match self.call(host, &f, Value::Undefined, &[value]) {
                Ok(v) => pass(self, host, true, v),
                Err(Ctrl::Throw(e)) => pass(self, host, false, e),
                Err(Ctrl::Abort(why)) => Err(Ctrl::Abort(why)),
                Err(_) => pass(self, host, true, Value::Undefined),
            },
            Handler::Finally(f) => match self.call(host, &f, Value::Undefined, &[]) {
                Err(Ctrl::Throw(e)) => pass(self, host, false, e),
                Err(Ctrl::Abort(why)) => Err(Ctrl::Abort(why)),
                _ => pass(self, host, ok, value),
            },
            Handler::Resume(n) => {
                self.resume(host, n, Some(if ok { Ok(value) } else { Err(value) }))
            }
            Handler::Element(n, i) => {
                let Some(agg) = self.aggregates.get_mut(n) else {
                    return Ok(());
                };
                let (target, mode) = (agg.result, agg.mode);
                match (mode, ok) {
                    (Mode::All, false) => {
                        self.settle(target, false, value);
                        return Ok(());
                    }
                    (Mode::Any, true) => {
                        return self.resolve_promise(host, target, value);
                    }
                    (Mode::AllSettled, _) => {
                        let o = self.new_object()?;
                        let status = if ok { "fulfilled" } else { "rejected" };
                        self.put(host, &o, "status", Value::str(status))?;
                        self.put(host, &o, if ok { "value" } else { "reason" }, value)?;
                        self.aggregates[n].values[i] = o;
                    }
                    _ => self.aggregates[n].values[i] = value,
                }
                let agg = &mut self.aggregates[n];
                agg.remaining -= 1;
                if agg.remaining > 0 {
                    return Ok(());
                }
                let values = std::mem::take(&mut agg.values);
                let all = self.new_array(values)?;
                if mode == Mode::Any {
                    let e = self.make_error("AggregateError", "all promises were rejected")?;
                    self.put(host, &e, "errors", all)?;
                    self.settle(target, false, e);
                } else {
                    self.settle(target, true, all);
                }
                Ok(())
            }
        }
    }

    /// Rejected promises nothing reacted to, since the last call.
    pub fn take_unhandled(&mut self) -> Vec<Value> {
        let list = std::mem::take(&mut self.unhandled);
        list.into_iter()
            .filter_map(|p| match &self.objects[p as usize].kind {
                Kind::Promise(d) if !d.handled => match &d.state {
                    PromiseState::Rejected(v) => Some(v.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// The value of `await v` where the statement machine did not settle it first: a settled
    /// promise's value, or `v` itself when it is not a promise.
    pub(super) fn await_now(&mut self, v: Value) -> Result<Value, Ctrl> {
        let Some(p) = self.is_promise(&v) else {
            return Ok(v);
        };
        let Some(data) = self.promise_data(p) else {
            return Ok(v);
        };
        data.handled = true;
        match &data.state {
            PromiseState::Fulfilled(x) => Ok(x.clone()),
            PromiseState::Rejected(x) => Err(Ctrl::Throw(x.clone())),
            PromiseState::Pending(_) => self.type_error(
                "await of a pending promise inside a condition or nested function is not supported",
            ),
        }
    }

    // ---- async functions ----

    /// Start an `async` call whose scope `env` holds its arguments: run it to its first
    /// `await` and return its promise.
    pub(super) fn call_async(
        &mut self,
        host: &mut dyn Host,
        env: EnvId,
        func: &Arc<Func>,
    ) -> Result<Value, Ctrl> {
        let promise = self.new_promise()?;
        let n = self.free_slot()?;
        self.coros[n] = Slot::Paused(Coro {
            frames: vec![Frame::Seq {
                list: Stmts::Many(func.body.clone()),
                next: 0,
                env,
                label: None,
            }],
            promise: Some(promise),
            slots: HashMap::new(),
            delegate: None,
        });
        self.resume(host, n, None)?;
        Ok(Value::Obj(promise))
    }

    /// Make a generator for a call whose scope `env` holds its arguments. Nothing runs until
    /// its first `next()`.
    pub(super) fn call_generator(&mut self, env: EnvId, func: &Arc<Func>) -> Result<Value, Ctrl> {
        let n = self.free_slot()?;
        self.coros[n] = Slot::Paused(Coro {
            frames: vec![Frame::Seq {
                list: Stmts::Many(func.body.clone()),
                next: 0,
                env,
                label: None,
            }],
            promise: None,
            slots: HashMap::new(),
            delegate: None,
        });
        // Its scopes outlive this call.
        self.pinned = self.pinned.max(self.envs.len());
        let proto = self.protos.generator;
        let g = self.alloc(
            Some(proto),
            Kind::Generator {
                slot: n,
                started: false,
                done: false,
            },
        )?;
        Ok(Value::Obj(g))
    }

    fn free_slot(&mut self) -> Result<usize, Ctrl> {
        match self.coros.iter().position(|s| matches!(s, Slot::Free)) {
            Some(free) => Ok(free),
            None if self.coros.len() >= PAUSED_LIMIT => {
                Err(Ctrl::Abort("too many paused functions".into()))
            }
            None => {
                self.coros.push(Slot::Free);
                Ok(self.coros.len() - 1)
            }
        }
    }

    /// `generator.next(v)`, `.throw(e)` or `.return(v)`: the `{ value, done }` it gives.
    pub fn generator_resume(
        &mut self,
        host: &mut dyn Host,
        this: &Value,
        how: Resume,
    ) -> Result<Value, Ctrl> {
        let g = match this {
            Value::Obj(g) if matches!(self.objects[*g as usize].kind, Kind::Generator { .. }) => *g,
            _ => return self.type_error("not a generator"),
        };
        let Kind::Generator {
            slot,
            started,
            done,
        } = &mut self.objects[g as usize].kind
        else {
            unreachable!()
        };
        let (n, was_started, was_done) = (*slot, *started, *done);
        let finish = |it: &mut Interp| {
            if let Kind::Generator { done, .. } = &mut it.objects[g as usize].kind {
                *done = true;
            }
            if !was_done {
                it.coros[n] = Slot::Free;
            }
        };
        if !was_done && matches!(self.coros.get(n), Some(Slot::Running)) {
            return self.type_error("the generator is already running");
        }
        let input = match how {
            Resume::Return(v) => {
                finish(self);
                return self.iter_result(v, true);
            }
            _ if was_done => {
                return match how {
                    Resume::Throw(e) => Err(Ctrl::Throw(e)),
                    _ => self.iter_result(Value::Undefined, true),
                };
            }
            Resume::Throw(e) if !was_started => {
                finish(self);
                return Err(Ctrl::Throw(e));
            }
            // The first `next` starts the body; what it passes goes nowhere.
            Resume::Next(_) if !was_started => None,
            Resume::Next(v) => Some(Ok(v)),
            Resume::Throw(e) => Some(Err(e)),
        };
        *started = true;
        let Slot::Paused(mut coro) = std::mem::replace(&mut self.coros[n], Slot::Running) else {
            return self.type_error("the generator is gone");
        };
        let outer = std::mem::replace(&mut self.slots, std::mem::take(&mut coro.slots));
        let r = self.drive(host, &mut coro, input);
        coro.slots = std::mem::replace(&mut self.slots, outer);
        let step = match r {
            Ok(step) => step,
            Err(e) => {
                finish(self);
                return Err(e);
            }
        };
        match step {
            Step::Yield(v) => {
                self.pinned = self.pinned.max(self.envs.len());
                self.coros[n] = Slot::Paused(coro);
                self.iter_result(v, false)
            }
            Step::Done(Completion::Return(v)) => {
                finish(self);
                self.iter_result(v, true)
            }
            Step::Done(Completion::Throw(e)) => {
                finish(self);
                Err(Ctrl::Throw(e))
            }
            Step::Pause(_) => {
                finish(self);
                self.type_error("await inside a generator is not supported")
            }
            _ => {
                finish(self);
                self.iter_result(Value::Undefined, true)
            }
        }
    }

    fn iter_result(&mut self, value: Value, done: bool) -> Result<Value, Ctrl> {
        let o = self.new_object()?;
        let Value::Obj(id) = o else { unreachable!() };
        self.set_own(id, "value", value);
        self.set_own(id, "done", Value::Bool(done));
        Ok(o)
    }

    /// Run paused call `n` until it pauses again or ends; `input` settles the `await` it
    /// paused at.
    fn resume(
        &mut self,
        host: &mut dyn Host,
        n: usize,
        input: Option<Result<Value, Value>>,
    ) -> Result<(), Ctrl> {
        let Some(slot) = self.coros.get_mut(n) else {
            return Ok(());
        };
        let Slot::Paused(mut coro) = std::mem::replace(slot, Slot::Running) else {
            return Ok(());
        };
        let outer = std::mem::replace(&mut self.slots, std::mem::take(&mut coro.slots));
        let r = self.drive(host, &mut coro, input);
        coro.slots = std::mem::replace(&mut self.slots, outer);
        match r? {
            Step::Pause(p) => {
                // Its scopes outlive the calls that made them.
                self.pinned = self.pinned.max(self.envs.len());
                self.subscribe(p, Handler::Resume(n), Handler::Resume(n), None);
                self.coros[n] = Slot::Paused(coro);
            }
            Step::Done(c) => {
                self.coros[n] = Slot::Free;
                let Some(promise) = coro.promise else {
                    return Ok(());
                };
                match c {
                    Completion::Throw(e) => self.settle(promise, false, e),
                    Completion::Return(v) => self.resolve_promise(host, promise, v)?,
                    _ => self.settle(promise, true, Value::Undefined),
                }
            }
            Step::Pushed | Step::Yield(_) => self.coros[n] = Slot::Free,
        }
        Ok(())
    }

    fn drive(
        &mut self,
        host: &mut dyn Host,
        coro: &mut Coro,
        mut input: Option<Result<Value, Value>>,
    ) -> Result<Step, Ctrl> {
        let mut pending: Option<Completion> = None;
        loop {
            if let Some(c) = pending.take() {
                match self.unwind(host, &mut coro.frames, c)? {
                    Some(c) => return Ok(Step::Done(c)),
                    None => continue,
                }
            }
            let Some(top) = coro.frames.last_mut() else {
                return Ok(Step::Done(Completion::Normal));
            };
            self.step()?;
            let step = match top {
                Frame::Seq {
                    list, next, env, ..
                } => {
                    if *next >= list.len() {
                        coro.frames.pop();
                        continue;
                    }
                    let at = At {
                        list: list.clone(),
                        i: *next,
                    };
                    *next += 1;
                    let env = *env;
                    self.start(host, &mut coro.frames, at, env, None)?
                }
                Frame::Prepare {
                    at,
                    env,
                    then,
                    next,
                } => {
                    let (at, env, then) = (at.clone(), *env, *then);
                    let list = prepared(at.stmt(), then);
                    if let Some(r) = input.take() {
                        // Inside a `yield*`, what `next` passes goes nowhere.
                        if coro.delegate.is_none() {
                            if let Some(e) = list.get(*next) {
                                self.slots.insert(key(e), r);
                            }
                            *next += 1;
                        }
                    }
                    if let Some(e) = list.get(*next) {
                        let value =
                            |it: &mut Interp, host: &mut dyn Host, v: &Option<Box<Expr>>| match v {
                                Some(v) => it.eval(host, env, v),
                                None => Ok(Value::Undefined),
                            };
                        match e {
                            Expr::Await(operand) => match self.eval(host, env, operand) {
                                Ok(v) => Step::Pause(self.promise_resolved(host, v)?),
                                Err(c) => Step::Done(completion(Err(c))?),
                            },
                            Expr::Yield(v, false) => match value(self, host, v) {
                                Ok(v) => Step::Yield(v),
                                Err(c) => Step::Done(completion(Err(c))?),
                            },
                            Expr::Yield(v, true) => {
                                if coro.delegate.is_none() {
                                    let items = value(self, host, v)
                                        .and_then(|inner| self.iterate(host, &inner));
                                    match items {
                                        Ok(items) => coro.delegate = Some((items, 0)),
                                        Err(c) => {
                                            pending = Some(completion(Err(c))?);
                                            continue;
                                        }
                                    }
                                }
                                let Some((items, i)) = coro.delegate.as_mut() else {
                                    unreachable!()
                                };
                                match items.get(*i).cloned() {
                                    Some(x) => {
                                        *i += 1;
                                        Step::Yield(x)
                                    }
                                    None => {
                                        coro.delegate = None;
                                        self.slots.insert(key(e), Ok(Value::Undefined));
                                        *next += 1;
                                        Step::Pushed
                                    }
                                }
                            }
                            _ => unreachable!("only awaits and yields are prepared"),
                        }
                    } else {
                        coro.frames.pop();
                        match then {
                            Then::Run => {
                                let r = self.exec(host, env, at.stmt());
                                Step::Done(completion(r)?)
                            }
                            Then::Start => self.compound(host, &mut coro.frames, at, env, None)?,
                            Then::Test => self.loop_step(host, &mut coro.frames, true)?,
                        }
                    }
                }
                Frame::Loop { .. } => self.loop_step(host, &mut coro.frames, false)?,
                Frame::Switch {
                    at,
                    env,
                    case,
                    next,
                } => {
                    let Stmt::Switch(_, cases) = at.stmt() else {
                        unreachable!()
                    };
                    match cases.get(*case) {
                        None => {
                            coro.frames.pop();
                            continue;
                        }
                        Some((_, body)) if *next >= body.len() => {
                            *case += 1;
                            *next = 0;
                            continue;
                        }
                        Some((_, body)) => {
                            let inner = At {
                                list: Stmts::Many(body.clone()),
                                i: *next,
                            };
                            *next += 1;
                            let env = *env;
                            self.start(host, &mut coro.frames, inner, env, None)?
                        }
                    }
                }
                Frame::Try {
                    at,
                    env,
                    phase,
                    saved,
                } => {
                    // The block on top of it finished normally.
                    let Stmt::Try(_, _, finalizer) = at.stmt() else {
                        unreachable!()
                    };
                    let finalizer = finalizer.clone();
                    let env = *env;
                    match (&phase, finalizer) {
                        (TryPhase::Finally, _) => {
                            let saved = saved.take();
                            coro.frames.pop();
                            match saved {
                                Some(c) => Step::Done(c),
                                None => continue,
                            }
                        }
                        (_, Some(f)) => {
                            *phase = TryPhase::Finally;
                            self.push_block(&mut coro.frames, Stmts::Many(f), env)?;
                            Step::Pushed
                        }
                        (_, None) => {
                            coro.frames.pop();
                            continue;
                        }
                    }
                }
            };
            match step {
                Step::Done(Completion::Normal) | Step::Pushed => {}
                Step::Done(c) => pending = Some(c),
                Step::Pause(p) => return Ok(Step::Pause(p)),
                Step::Yield(v) => return Ok(Step::Yield(v)),
            }
        }
    }

    /// Pass an abrupt completion down the frames. Returns it when it leaves the function.
    fn unwind(
        &mut self,
        host: &mut dyn Host,
        frames: &mut Vec<Frame>,
        c: Completion,
    ) -> Result<Option<Completion>, Ctrl> {
        let mut c = c;
        while let Some(top) = frames.last_mut() {
            match top {
                Frame::Seq { label, .. } => {
                    if let (Completion::Break(Some(l)), Some(own)) = (&c, label.as_ref()) {
                        if l == own {
                            frames.pop();
                            return Ok(None);
                        }
                    }
                    frames.pop();
                }
                Frame::Prepare { .. } => {
                    frames.pop();
                }
                Frame::Loop { label, phase, .. } => {
                    let mine = |l: &Option<Name>| l.is_none() || *l == *label;
                    match &c {
                        Completion::Break(l) if mine(l) => {
                            frames.pop();
                            return Ok(None);
                        }
                        Completion::Continue(l) if mine(l) => {
                            *phase = Phase::AfterBody;
                            return Ok(None);
                        }
                        _ => {
                            frames.pop();
                        }
                    }
                }
                Frame::Switch { .. } => {
                    frames.pop();
                    if matches!(c, Completion::Break(None)) {
                        return Ok(None);
                    }
                }
                Frame::Try {
                    at,
                    env,
                    phase,
                    saved,
                } => {
                    let at = at.clone();
                    let Stmt::Try(_, handler, finalizer) = at.stmt() else {
                        unreachable!()
                    };
                    let (finalizer, env) = (finalizer.clone(), *env);
                    match (&phase, c, handler, finalizer) {
                        (TryPhase::Body, Completion::Throw(v), Some((param, body)), _) => {
                            *phase = TryPhase::Catch;
                            let mark_env = self.new_env(env, None)?;
                            if let Some(p) = &param {
                                self.bind(host, mark_env, p, v, Some(VarKind::Let))?;
                            }
                            self.push_block(frames, Stmts::Many(body.clone()), mark_env)?;
                            return Ok(None);
                        }
                        (TryPhase::Body | TryPhase::Catch, other, _, Some(f)) => {
                            *phase = TryPhase::Finally;
                            *saved = Some(other);
                            self.push_block(frames, Stmts::Many(f), env)?;
                            return Ok(None);
                        }
                        (_, other, _, _) => {
                            c = other;
                            frames.pop();
                        }
                    }
                }
            }
        }
        Ok(Some(c))
    }

    /// A block's own scope, with its function declarations, on top of the frames.
    fn push_block(&mut self, frames: &mut Vec<Frame>, list: Stmts, env: EnvId) -> Result<(), Ctrl> {
        let inner = self.new_env(env, None)?;
        if let Stmts::Many(body) = &list {
            for s in body.iter() {
                if let Stmt::Function(f) = s {
                    let v = self.closure(f, inner, None)?;
                    if let Some(name) = &f.name {
                        self.declare(inner, name, v, false);
                    }
                }
            }
        }
        frames.push(Frame::Seq {
            list,
            next: 0,
            env: inner,
            label: None,
        });
        Ok(())
    }

    /// Run statement `at`: whole when it cannot pause, else through frames.
    fn start(
        &mut self,
        host: &mut dyn Host,
        frames: &mut Vec<Frame>,
        at: At,
        env: EnvId,
        label: Option<Name>,
    ) -> Result<Step, Ctrl> {
        let s = at.stmt();
        if !suspends(s) {
            return Ok(Step::Done(completion(self.exec(host, env, s))?));
        }
        match s {
            Stmt::Expr(_) | Stmt::Var(..) | Stmt::Return(_) | Stmt::Throw(_) => {
                frames.push(Frame::Prepare {
                    at,
                    env,
                    then: Then::Run,
                    next: 0,
                });
                Ok(Step::Pushed)
            }
            Stmt::Block(body) => {
                let body = body.clone();
                self.push_block(frames, Stmts::Many(body), env)?;
                Ok(Step::Pushed)
            }
            Stmt::If(..) | Stmt::ForIn(..) | Stmt::Switch(..)
                if !prepared(s, Then::Start).is_empty() =>
            {
                frames.push(Frame::Prepare {
                    at,
                    env,
                    then: Then::Start,
                    next: 0,
                });
                Ok(Step::Pushed)
            }
            Stmt::Labeled(l, body) => {
                let (l, body) = (l.clone(), body.clone());
                let is_loop = matches!(
                    *body,
                    Stmt::While(..) | Stmt::DoWhile(..) | Stmt::For(..) | Stmt::ForIn(..)
                );
                let inner = At {
                    list: Stmts::One(body),
                    i: 0,
                };
                if is_loop {
                    self.start(host, frames, inner, env, Some(l))
                } else {
                    frames.push(Frame::Seq {
                        list: inner.list,
                        next: 0,
                        env,
                        label: Some(l),
                    });
                    Ok(Step::Pushed)
                }
            }
            _ => self.compound(host, frames, at, env, label),
        }
    }

    /// Start a compound statement whose head `await`s are settled.
    fn compound(
        &mut self,
        host: &mut dyn Host,
        frames: &mut Vec<Frame>,
        at: At,
        env: EnvId,
        label: Option<Name>,
    ) -> Result<Step, Ctrl> {
        let done = |r: Result<Value, Ctrl>| -> Result<Result<Value, Completion>, Ctrl> {
            match r {
                Ok(v) => Ok(Ok(v)),
                Err(c) => completion(Err(c)).map(Err),
            }
        };
        match at.stmt() {
            Stmt::If(test, then, otherwise) => {
                let (then, otherwise) = (then.clone(), otherwise.clone());
                let t = match done(self.eval(host, env, test))? {
                    Ok(t) => t,
                    Err(c) => return Ok(Step::Done(c)),
                };
                let branch = if self.truthy(&t) {
                    Some(then)
                } else {
                    otherwise
                };
                if let Some(b) = branch {
                    frames.push(Frame::Seq {
                        list: Stmts::One(b),
                        next: 0,
                        env,
                        label: None,
                    });
                }
                Ok(Step::Pushed)
            }
            Stmt::While(..) => {
                frames.push(Frame::Loop {
                    at,
                    env,
                    label,
                    kind: LoopKind::While,
                    phase: Phase::Next,
                });
                Ok(Step::Pushed)
            }
            Stmt::DoWhile(body, _) => {
                let body = body.clone();
                frames.push(Frame::Loop {
                    at,
                    env,
                    label,
                    kind: LoopKind::DoWhile,
                    phase: Phase::AfterBody,
                });
                frames.push(Frame::Seq {
                    list: Stmts::One(body),
                    next: 0,
                    env,
                    label: None,
                });
                Ok(Step::Pushed)
            }
            Stmt::For(init, ..) => {
                let scope = self.new_env(env, None)?;
                let per_pass = matches!(
                    init.as_deref(),
                    Some(Stmt::Var(VarKind::Let | VarKind::Const, _))
                );
                if let Some(i) = init {
                    let c = completion(self.exec(host, scope, i))?;
                    if !matches!(c, Completion::Normal) {
                        return Ok(Step::Done(c));
                    }
                }
                frames.push(Frame::Loop {
                    at,
                    env: scope,
                    label,
                    kind: LoopKind::For {
                        per_pass,
                        outer: env,
                    },
                    phase: Phase::Next,
                });
                Ok(Step::Pushed)
            }
            Stmt::ForIn(_, _, object, _, of) => {
                let of = *of;
                let o = match done(self.eval(host, env, object))? {
                    Ok(o) => o,
                    Err(c) => return Ok(Step::Done(c)),
                };
                let source = match self.for_source(host, &o, of) {
                    Ok(source) => source,
                    Err(c) => return Ok(Step::Done(completion(Err(c))?)),
                };
                frames.push(Frame::Loop {
                    at,
                    env,
                    label,
                    kind: LoopKind::Items(source),
                    phase: Phase::Next,
                });
                Ok(Step::Pushed)
            }
            Stmt::Switch(subject, cases) => {
                let v = match done(self.eval(host, env, subject))? {
                    Ok(v) => v,
                    Err(c) => return Ok(Step::Done(c)),
                };
                let mut start = None;
                for (i, (test, _)) in cases.iter().enumerate() {
                    if let Some(t) = test {
                        let t = match done(self.eval(host, env, t))? {
                            Ok(t) => t,
                            Err(c) => return Ok(Step::Done(c)),
                        };
                        if self.strict_equals(&v, &t) {
                            start = Some(i);
                            break;
                        }
                    }
                }
                let start = start.or_else(|| cases.iter().position(|(t, _)| t.is_none()));
                if let Some(case) = start {
                    let scope = self.new_env(env, None)?;
                    frames.push(Frame::Switch {
                        at,
                        env: scope,
                        case,
                        next: 0,
                    });
                }
                Ok(Step::Pushed)
            }
            Stmt::Try(body, ..) => {
                let body = body.clone();
                frames.push(Frame::Try {
                    at,
                    env,
                    phase: TryPhase::Body,
                    saved: None,
                });
                self.push_block(frames, Stmts::Many(body), env)?;
                Ok(Step::Pushed)
            }
            other => Ok(Step::Done(completion(self.exec(host, env, other))?)),
        }
    }

    /// The loop frame on top: run its next pass, or end it. `tested`: its test's `await`s
    /// were just settled.
    fn loop_step(
        &mut self,
        host: &mut dyn Host,
        frames: &mut Vec<Frame>,
        tested: bool,
    ) -> Result<Step, Ctrl> {
        let Some(Frame::Loop {
            at,
            env,
            kind,
            phase,
            ..
        }) = frames.last_mut()
        else {
            return Ok(Step::Done(Completion::Normal));
        };
        let (at, env_now) = (at.clone(), *env);
        // After the body: a `for` loop copies its `let`s for the next pass and updates.
        if let Phase::AfterBody = phase {
            *phase = Phase::Next;
            if let (LoopKind::For { per_pass, outer }, Stmt::For(_, _, update, _)) =
                (&*kind, at.stmt())
            {
                let (per_pass, outer) = (*per_pass, *outer);
                let mut current = env_now;
                if per_pass {
                    let vars = self.copy_vars(current);
                    current = self.new_env(outer, None)?;
                    self.set_vars(current, vars);
                    if let Some(Frame::Loop { env, .. }) = frames.last_mut() {
                        *env = current;
                    }
                }
                if let Some(u) = update {
                    let c = completion(self.eval(host, current, u).map(|_| ()))?;
                    if !matches!(c, Completion::Normal) {
                        return Ok(Step::Done(c));
                    }
                }
            }
        }
        let Some(Frame::Loop {
            env, kind, phase, ..
        }) = frames.last_mut()
        else {
            unreachable!()
        };
        let env = *env;
        let test = match at.stmt() {
            Stmt::While(t, _) | Stmt::DoWhile(_, t) => Some(t),
            Stmt::For(_, t, _, _) => t.as_ref(),
            _ => None,
        };
        let body = match at.stmt() {
            Stmt::While(_, b) | Stmt::DoWhile(b, _) | Stmt::For(_, _, _, b) => b.clone(),
            Stmt::ForIn(_, _, _, b, _) => b.clone(),
            _ => unreachable!(),
        };
        if let LoopKind::Items(source) = kind {
            *phase = Phase::AfterBody;
            let mut source = std::mem::replace(source, super::Source::List(Vec::new(), 0));
            let item = self.next_item(host, &mut source);
            if let Some(Frame::Loop {
                kind: LoopKind::Items(s),
                ..
            }) = frames.last_mut()
            {
                *s = source;
            }
            let item = match item {
                Ok(Some(item)) => item,
                Ok(None) => {
                    frames.pop();
                    return Ok(Step::Pushed);
                }
                Err(c) => return Ok(Step::Done(completion(Err(c))?)),
            };
            let Stmt::ForIn(decl, pattern, ..) = at.stmt() else {
                unreachable!()
            };
            let scope = self.new_env(env, None)?;
            let k = decl.unwrap_or(VarKind::Var);
            let r = if decl.is_some() && k != VarKind::Var {
                self.bind(host, scope, pattern, item, Some(k))
            } else {
                self.bind(host, env, pattern, item, None)
            };
            let c = completion(r)?;
            if !matches!(c, Completion::Normal) {
                return Ok(Step::Done(c));
            }
            frames.push(Frame::Seq {
                list: Stmts::One(body),
                next: 0,
                env: scope,
                label: None,
            });
            return Ok(Step::Pushed);
        }
        if let Some(t) = test {
            if !tested && !prepared(at.stmt(), Then::Test).is_empty() {
                frames.push(Frame::Prepare {
                    at,
                    env,
                    then: Then::Test,
                    next: 0,
                });
                return Ok(Step::Pushed);
            }
            let v = match self.eval(host, env, t) {
                Ok(v) => v,
                Err(c) => return Ok(Step::Done(completion(Err(c))?)),
            };
            if !self.truthy(&v) {
                frames.pop();
                return Ok(Step::Pushed);
            }
        }
        if let Some(Frame::Loop { phase, .. }) = frames.last_mut() {
            *phase = Phase::AfterBody;
        }
        frames.push(Frame::Seq {
            list: Stmts::One(body),
            next: 0,
            env,
            label: None,
        });
        Ok(Step::Pushed)
    }
}
