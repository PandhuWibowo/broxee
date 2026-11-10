//! The interpreter: values, a per-page heap, scopes, and evaluation of the syntax tree.
//!
//! Objects and scopes live in arenas owned by the page's `Interp` and refer to each other by
//! index, so reference cycles (a closure and the scope it closes over) cost nothing to free:
//! the whole heap goes when the page goes. There is no collector while the page lives;
//! instead the heap has a ceiling, and scopes of calls that created no closure are popped
//! when the call returns. A script that runs too long or allocates too much is stopped and
//! scripts are turned off for the page.

use std::collections::HashMap;
use std::sync::Arc;

use crate::parser::{Elem, Expr, Func, Name, Param, Pattern, Prop, Stmt, VarKind};
use crate::Host;

pub type ObjId = u32;
pub type EnvId = u32;

/// Steps (expressions and statements) one script, handler or timer may take.
pub const STEP_LIMIT: u64 = 20_000_000;
/// Objects a page may create. About 100 bytes each with a few properties.
pub const OBJECT_LIMIT: usize = 150_000;
const ENV_LIMIT: usize = 400_000;
const STRING_LIMIT: usize = 1 << 20;
const DEPTH_LIMIT: usize = 400;

#[derive(Clone, Debug)]
pub enum Value {
    Undefined,
    Null,
    Bool(bool),
    Num(f64),
    Str(Arc<str>),
    Obj(ObjId),
}

impl Value {
    pub fn str(s: &str) -> Value {
        Value::Str(s.into())
    }

    pub fn is_nullish(&self) -> bool {
        matches!(self, Value::Undefined | Value::Null)
    }
}

pub type NativeFn = fn(&mut Interp, &mut dyn Host, Value, &[Value]) -> Result<Value, Ctrl>;

pub enum Kind {
    Plain,
    Array(Vec<Value>),
    /// A function written in script, the scope it closes over, and (for arrows) its `this`.
    Function(Arc<Func>, EnvId, Option<Value>),
    Native(NativeFn),
    /// A function with `this` and leading arguments fixed (`bind`).
    Bound(ObjId, Value, Vec<Value>),
    Element(u32),
    ClassList(u32),
    Style(u32),
    Error,
    RegExp(Arc<crate::regex::Regex>),
}

pub struct Object {
    pub proto: Option<ObjId>,
    pub props: Vec<(Arc<str>, Value)>,
    pub kind: Kind,
}

struct Binding {
    name: Name,
    value: Value,
    constant: bool,
}

struct Env {
    vars: Vec<Binding>,
    parent: Option<EnvId>,
    /// `this` of a function scope.
    this: Option<Value>,
}

/// Why evaluation stopped early.
#[derive(Debug)]
pub enum Ctrl {
    Return(Value),
    Break(Option<Name>),
    Continue(Option<Name>),
    Throw(Value),
    /// The page's scripts are stopped (too long, too much memory).
    Abort(String),
}

#[derive(Default, Clone, Copy)]
pub struct Protos {
    pub object: ObjId,
    pub function: ObjId,
    pub array: ObjId,
    pub string: ObjId,
    pub number: ObjId,
    pub boolean: ObjId,
    pub error: ObjId,
    pub element: ObjId,
    pub class_list: ObjId,
    pub style: ObjId,
    pub event: ObjId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    Node(u32),
    Window,
}

pub struct Timer {
    pub id: u32,
    pub due: f64,
    pub every: Option<f64>,
    pub callback: Value,
    pub args: Vec<Value>,
}

pub struct Interp {
    pub objects: Vec<Object>,
    envs: Vec<Env>,
    /// Scopes below this index are referenced by some closure and must stay.
    pinned: usize,
    pub global: ObjId,
    pub global_env: EnvId,
    pub protos: Protos,
    pub elements: HashMap<u32, ObjId>,
    pub listeners: Vec<(Target, Arc<str>, Value)>,
    pub timers: Vec<Timer>,
    pub next_timer: u32,
    pub steps: u64,
    depth: usize,
    /// Milliseconds since the page started, for timers and `Date.now`.
    pub now: f64,
    pub epoch_ms: f64,
    /// Set when the page's scripts were stopped, with the reason.
    pub stopped: Option<String>,
    pub storage: Vec<(String, String)>,
    /// The label of the loop about to run (`outer: for …`), for `continue outer`.
    pending_label: Option<Name>,
}

pub fn number_to_string(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if v == 0.0 {
        return "0".into();
    }
    let a = v.abs();
    if v.fract() == 0.0 && a < 1e21 {
        return format!("{}", v as i128);
    }
    if !(1e-6..1e21).contains(&a) {
        let s = format!("{v:e}");
        // JavaScript writes the sign of a positive exponent.
        return match s.split_once('e') {
            Some((m, e)) if !e.starts_with('-') => format!("{m}e+{e}"),
            _ => s,
        };
    }
    format!("{v}")
}

/// `ToNumber` for a string.
pub fn parse_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        return 0.0;
    }
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return u64::from_str_radix(h, 16).map_or(f64::NAN, |v| v as f64);
    }
    match t {
        "Infinity" | "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        _ if t
            .bytes()
            .all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)) =>
        {
            t.parse().unwrap_or(f64::NAN)
        }
        _ => f64::NAN,
    }
}

fn to_int32(v: f64) -> i32 {
    if !v.is_finite() {
        return 0;
    }
    (v.trunc() as i64 as u64 & 0xffff_ffff) as u32 as i32
}

impl Interp {
    pub fn new() -> Interp {
        let mut it = Interp {
            objects: Vec::new(),
            envs: Vec::new(),
            pinned: 1,
            global: 0,
            global_env: 0,
            protos: Protos::default(),
            elements: HashMap::new(),
            listeners: Vec::new(),
            timers: Vec::new(),
            next_timer: 1,
            steps: 0,
            depth: 0,
            now: 0.0,
            epoch_ms: 0.0,
            stopped: None,
            storage: Vec::new(),
            pending_label: None,
        };
        let object = it.raw_alloc(None, Kind::Plain);
        let mut proto = |kind| it.raw_alloc(Some(object), kind);
        let function = proto(Kind::Plain);
        let array = proto(Kind::Array(Vec::new()));
        let string = proto(Kind::Plain);
        let number = proto(Kind::Plain);
        let boolean = proto(Kind::Plain);
        let error = proto(Kind::Plain);
        let element = proto(Kind::Plain);
        let class_list = proto(Kind::Plain);
        let style = proto(Kind::Plain);
        let event = proto(Kind::Plain);
        it.protos = Protos {
            object,
            function,
            array,
            string,
            number,
            boolean,
            error,
            element,
            class_list,
            style,
            event,
        };
        it.global = it.raw_alloc(Some(object), Kind::Plain);
        it.envs.push(Env {
            vars: Vec::new(),
            parent: None,
            this: Some(Value::Obj(it.global)),
        });
        crate::builtins::install(&mut it);
        it
    }

    fn raw_alloc(&mut self, proto: Option<ObjId>, kind: Kind) -> ObjId {
        self.objects.push(Object {
            proto,
            props: Vec::new(),
            kind,
        });
        (self.objects.len() - 1) as ObjId
    }

    pub fn alloc(&mut self, proto: Option<ObjId>, kind: Kind) -> Result<ObjId, Ctrl> {
        if self.objects.len() >= OBJECT_LIMIT {
            return Err(Ctrl::Abort("scripts created too many objects".into()));
        }
        Ok(self.raw_alloc(proto, kind))
    }

    pub fn new_object(&mut self) -> Result<Value, Ctrl> {
        let p = self.protos.object;
        Ok(Value::Obj(self.alloc(Some(p), Kind::Plain)?))
    }

    pub fn new_array(&mut self, items: Vec<Value>) -> Result<Value, Ctrl> {
        let p = self.protos.array;
        Ok(Value::Obj(self.alloc(Some(p), Kind::Array(items))?))
    }

    pub fn native(&mut self, f: NativeFn) -> ObjId {
        let p = self.protos.function;
        self.raw_alloc(Some(p), Kind::Native(f))
    }

    /// Put a native method `name` on object `on`.
    pub fn method(&mut self, on: ObjId, name: &str, f: NativeFn) {
        let id = self.native(f);
        self.set_own(on, name, Value::Obj(id));
    }

    pub fn set_own(&mut self, on: ObjId, key: &str, value: Value) {
        let props = &mut self.objects[on as usize].props;
        match props.iter_mut().find(|(k, _)| &**k == key) {
            Some(slot) => slot.1 = value,
            None => props.push((key.into(), value)),
        }
    }

    pub fn get_own(&self, on: ObjId, key: &str) -> Option<Value> {
        self.objects[on as usize]
            .props
            .iter()
            .find(|(k, _)| &**k == key)
            .map(|(_, v)| v.clone())
    }

    pub fn throw_error(&mut self, kind: &str, message: &str) -> Ctrl {
        match self.make_error(kind, message) {
            Ok(v) => Ctrl::Throw(v),
            Err(c) => c,
        }
    }

    pub fn make_error(&mut self, kind: &str, message: &str) -> Result<Value, Ctrl> {
        let p = self.protos.error;
        let e = self.alloc(Some(p), Kind::Error)?;
        self.set_own(e, "name", Value::str(kind));
        self.set_own(e, "message", Value::str(message));
        Ok(Value::Obj(e))
    }

    pub fn type_error<T>(&mut self, message: &str) -> Result<T, Ctrl> {
        Err(self.throw_error("TypeError", message))
    }

    fn step(&mut self) -> Result<(), Ctrl> {
        self.steps += 1;
        if self.steps > STEP_LIMIT {
            return Err(Ctrl::Abort("a script ran too long".into()));
        }
        Ok(())
    }

    // ---- Conversions ----

    pub fn truthy(&self, v: &Value) -> bool {
        match v {
            Value::Undefined | Value::Null => false,
            Value::Bool(b) => *b,
            Value::Num(n) => *n != 0.0 && !n.is_nan(),
            Value::Str(s) => !s.is_empty(),
            Value::Obj(_) => true,
        }
    }

    pub fn is_callable(&self, v: &Value) -> bool {
        matches!(v, Value::Obj(o) if matches!(self.objects[*o as usize].kind, Kind::Function(..) | Kind::Native(_) | Kind::Bound(..)))
    }

    pub fn to_number(&mut self, host: &mut dyn Host, v: &Value) -> Result<f64, Ctrl> {
        Ok(match v {
            Value::Undefined => f64::NAN,
            Value::Null => 0.0,
            Value::Bool(b) => f64::from(u8::from(*b)),
            Value::Num(n) => *n,
            Value::Str(s) => parse_number(s),
            Value::Obj(_) => {
                let p = self.to_primitive(host, v)?;
                return self.to_number(host, &p);
            }
        })
    }

    pub fn to_string(&mut self, host: &mut dyn Host, v: &Value) -> Result<Arc<str>, Ctrl> {
        Ok(match v {
            Value::Undefined => "undefined".into(),
            Value::Null => "null".into(),
            Value::Bool(b) => if *b { "true" } else { "false" }.into(),
            Value::Num(n) => number_to_string(*n).into(),
            Value::Str(s) => s.clone(),
            Value::Obj(_) => {
                let p = self.to_primitive(host, v)?;
                return self.to_string(host, &p);
            }
        })
    }

    /// Objects to primitives: `toString()` (arrays join, errors show their message).
    pub fn to_primitive(&mut self, host: &mut dyn Host, v: &Value) -> Result<Value, Ctrl> {
        let Value::Obj(o) = v else {
            return Ok(v.clone());
        };
        if let Some(f) = self.get(host, v, "valueOf")? {
            if self.is_callable(&f)
                && !matches!(
                    self.objects[*o as usize].kind,
                    Kind::Plain | Kind::Array(_) | Kind::Error | Kind::Element(_)
                )
            {
                let r = self.call(host, &f, v.clone(), &[])?;
                if !matches!(r, Value::Obj(_)) {
                    return Ok(r);
                }
            }
        }
        if let Some(f) = self.get(host, v, "toString")? {
            if self.is_callable(&f) {
                let r = self.call(host, &f, v.clone(), &[])?;
                if !matches!(r, Value::Obj(_)) {
                    return Ok(r);
                }
            }
        }
        Ok(Value::str("[object Object]"))
    }

    pub fn type_of(&self, v: &Value) -> &'static str {
        match v {
            Value::Undefined => "undefined",
            Value::Null => "object",
            Value::Bool(_) => "boolean",
            Value::Num(_) => "number",
            Value::Str(_) => "string",
            Value::Obj(_) if self.is_callable(v) => "function",
            Value::Obj(_) => "object",
        }
    }

    pub fn strict_equals(&self, a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Undefined, Value::Undefined) | (Value::Null, Value::Null) => true,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Num(x), Value::Num(y)) => x == y,
            (Value::Str(x), Value::Str(y)) => x == y,
            (Value::Obj(x), Value::Obj(y)) => x == y,
            _ => false,
        }
    }

    fn loose_equals(&mut self, host: &mut dyn Host, a: &Value, b: &Value) -> Result<bool, Ctrl> {
        Ok(match (a, b) {
            (x, y) if x.is_nullish() && y.is_nullish() => true,
            (x, y) if x.is_nullish() || y.is_nullish() => false,
            (Value::Obj(_), Value::Obj(_)) => self.strict_equals(a, b),
            (Value::Obj(_), _) => {
                let p = self.to_primitive(host, a)?;
                return self.loose_equals(host, &p, b);
            }
            (_, Value::Obj(_)) => {
                let p = self.to_primitive(host, b)?;
                return self.loose_equals(host, a, &p);
            }
            (Value::Str(x), Value::Str(y)) => x == y,
            _ => self.to_number(host, a)? == self.to_number(host, b)?,
        })
    }

    // ---- Properties ----

    /// The prototype a value's properties are looked up on.
    fn proto_of(&self, v: &Value) -> Option<ObjId> {
        match v {
            Value::Str(_) => Some(self.protos.string),
            Value::Num(_) => Some(self.protos.number),
            Value::Bool(_) => Some(self.protos.boolean),
            Value::Obj(o) => Some(*o),
            _ => None,
        }
    }

    pub fn get(
        &mut self,
        host: &mut dyn Host,
        v: &Value,
        key: &str,
    ) -> Result<Option<Value>, Ctrl> {
        match v {
            Value::Str(s) => {
                if key == "length" {
                    return Ok(Some(Value::Num(s.chars().count() as f64)));
                }
                if let Ok(i) = key.parse::<usize>() {
                    return Ok(s.chars().nth(i).map(|c| Value::str(&c.to_string())));
                }
            }
            Value::Obj(o) => match &self.objects[*o as usize].kind {
                Kind::Array(items) => {
                    if key == "length" {
                        return Ok(Some(Value::Num(items.len() as f64)));
                    }
                    if let Ok(i) = key.parse::<usize>() {
                        return Ok(Some(items.get(i).cloned().unwrap_or(Value::Undefined)));
                    }
                }
                Kind::Function(f, ..) if key == "length" => {
                    return Ok(Some(Value::Num(f.params.len() as f64)));
                }
                Kind::Function(f, ..) if key == "name" => {
                    return Ok(Some(Value::str(f.name.as_deref().unwrap_or(""))));
                }
                Kind::Element(n) => {
                    let n = *n;
                    if let Some(v) = crate::builtins::element_get(self, host, n, key)? {
                        return Ok(Some(v));
                    }
                }
                Kind::Style(n) => {
                    let n = *n;
                    if self.get_own(*o, key).is_none()
                        && !key.starts_with("set")
                        && key != "getPropertyValue"
                        && key != "removeProperty"
                    {
                        return Ok(Some(Value::str(&crate::builtins::style_get(host, n, key))));
                    }
                }
                _ => {}
            },
            Value::Undefined | Value::Null => {
                let what = if matches!(v, Value::Null) {
                    "null"
                } else {
                    "undefined"
                };
                return self.type_error(&format!("cannot read property '{key}' of {what}"));
            }
            _ => {}
        }
        let mut at = self.proto_of(v);
        while let Some(o) = at {
            if let Some(found) = self.get_own(o, key) {
                return Ok(Some(found));
            }
            at = self.objects[o as usize].proto;
        }
        Ok(None)
    }

    pub fn get_value(&mut self, host: &mut dyn Host, v: &Value, key: &str) -> Result<Value, Ctrl> {
        Ok(self.get(host, v, key)?.unwrap_or(Value::Undefined))
    }

    pub fn put(
        &mut self,
        host: &mut dyn Host,
        target: &Value,
        key: &str,
        value: Value,
    ) -> Result<(), Ctrl> {
        let Value::Obj(o) = target else {
            if target.is_nullish() {
                return self.type_error(&format!(
                    "cannot set property '{key}' of {}",
                    self.type_of(target)
                ));
            }
            return Ok(());
        };
        let o = *o;
        match &mut self.objects[o as usize].kind {
            Kind::Array(items) => {
                if key == "length" {
                    let n = self.to_number(host, &value)?;
                    if let Kind::Array(items) = &mut self.objects[o as usize].kind {
                        items.resize(
                            if n.is_nan() {
                                0
                            } else {
                                n.clamp(0.0, 1e7) as usize
                            },
                            Value::Undefined,
                        );
                    }
                    return Ok(());
                }
                if let Ok(i) = key.parse::<usize>() {
                    if i > 10_000_000 {
                        return Err(Ctrl::Abort("an array grew too large".into()));
                    }
                    if i >= items.len() {
                        items.resize(i + 1, Value::Undefined);
                    }
                    items[i] = value;
                    return Ok(());
                }
            }
            Kind::Element(n) => {
                let n = *n;
                if crate::builtins::element_set(self, host, n, key, &value)? {
                    return Ok(());
                }
            }
            Kind::Style(n) => {
                let n = *n;
                let v = self.to_string(host, &value)?;
                crate::builtins::style_set(host, n, key, &v);
                return Ok(());
            }
            _ => {}
        }
        self.set_own(o, key, value);
        Ok(())
    }

    pub fn property_key(&mut self, host: &mut dyn Host, v: &Value) -> Result<Arc<str>, Ctrl> {
        match v {
            Value::Num(n) if n.fract() == 0.0 && *n >= 0.0 => Ok(format!("{}", *n as u64).into()),
            _ => self.to_string(host, v),
        }
    }

    /// Own enumerable keys, in order (array indices first).
    pub fn keys(&self, v: &Value) -> Vec<Arc<str>> {
        let Value::Obj(o) = v else {
            if let Value::Str(s) = v {
                return (0..s.chars().count())
                    .map(|i| i.to_string().into())
                    .collect();
            }
            return Vec::new();
        };
        let obj = &self.objects[*o as usize];
        let mut out: Vec<Arc<str>> = match &obj.kind {
            Kind::Array(items) => (0..items.len()).map(|i| i.to_string().into()).collect(),
            _ => Vec::new(),
        };
        out.extend(obj.props.iter().map(|(k, _)| k.clone()));
        out
    }

    /// The items of an iterable: arrays, strings, array-likes with a `length`.
    pub fn iterate(&mut self, host: &mut dyn Host, v: &Value) -> Result<Vec<Value>, Ctrl> {
        match v {
            Value::Str(s) => Ok(s.chars().map(|c| Value::str(&c.to_string())).collect()),
            Value::Obj(o) => {
                if let Kind::Array(items) = &self.objects[*o as usize].kind {
                    return Ok(items.clone());
                }
                let len = self.get_value(host, v, "length")?;
                let n = self.to_number(host, &len)?;
                if n.is_nan() {
                    return self.type_error("value is not iterable");
                }
                let mut out = Vec::new();
                for i in 0..(n as usize).min(10_000_000) {
                    out.push(self.get_value(host, v, &i.to_string())?);
                }
                Ok(out)
            }
            _ => self.type_error("value is not iterable"),
        }
    }

    // ---- Scopes ----

    fn new_env(&mut self, parent: EnvId, this: Option<Value>) -> Result<EnvId, Ctrl> {
        if self.envs.len() >= ENV_LIMIT {
            return Err(Ctrl::Abort("scripts used too much memory".into()));
        }
        self.envs.push(Env {
            vars: Vec::new(),
            parent: Some(parent),
            this,
        });
        Ok((self.envs.len() - 1) as EnvId)
    }

    /// Drop scopes made since `mark` when no closure refers to them.
    fn release_envs(&mut self, mark: usize) {
        if self.pinned <= mark && self.envs.len() > mark {
            self.envs.truncate(mark);
        }
    }

    fn declare(&mut self, env: EnvId, name: &Name, value: Value, constant: bool) {
        if env == self.global_env && !constant {
            // Global `var` and functions are properties of `window`.
            let g = self.global;
            self.set_own(g, name, value);
            return;
        }
        let vars = &mut self.envs[env as usize].vars;
        match vars.iter_mut().find(|b| b.name == *name) {
            Some(b) => {
                b.value = value;
                b.constant = constant;
            }
            None => vars.push(Binding {
                name: name.clone(),
                value,
                constant,
            }),
        }
    }

    fn lookup(&mut self, host: &mut dyn Host, env: EnvId, name: &str) -> Result<Value, Ctrl> {
        let mut e = Some(env);
        while let Some(id) = e {
            let scope = &self.envs[id as usize];
            if let Some(b) = scope.vars.iter().find(|b| &*b.name == name) {
                return Ok(b.value.clone());
            }
            e = scope.parent;
        }
        let g = Value::Obj(self.global);
        match self.get(host, &g, name)? {
            Some(v) => Ok(v),
            None if name == "undefined" => Ok(Value::Undefined),
            None => Err(self.throw_error("ReferenceError", &format!("{name} is not defined"))),
        }
    }

    fn assign_name(
        &mut self,
        host: &mut dyn Host,
        env: EnvId,
        name: &str,
        value: Value,
    ) -> Result<(), Ctrl> {
        let mut e = Some(env);
        while let Some(id) = e {
            let scope = &mut self.envs[id as usize];
            if let Some(b) = scope.vars.iter_mut().find(|b| &*b.name == name) {
                if b.constant {
                    return self.type_error(&format!("assignment to constant {name}"));
                }
                b.value = value;
                return Ok(());
            }
            e = scope.parent;
        }
        let g = Value::Obj(self.global);
        self.put(host, &g, name, value)
    }

    fn this_of(&self, env: EnvId) -> Value {
        let mut e = Some(env);
        while let Some(id) = e {
            if let Some(t) = &self.envs[id as usize].this {
                return t.clone();
            }
            e = self.envs[id as usize].parent;
        }
        Value::Undefined
    }

    // ---- Running ----

    /// Run a parsed program in the global scope.
    pub fn run_program(&mut self, host: &mut dyn Host, program: &[Stmt]) -> Result<(), Ctrl> {
        let env = self.global_env;
        self.hoist(env, program)?;
        for s in program {
            match self.exec(host, env, s) {
                Ok(()) => {}
                Err(Ctrl::Return(_) | Ctrl::Break(_) | Ctrl::Continue(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Function declarations and `var` names are defined before the code runs.
    fn hoist(&mut self, env: EnvId, body: &[Stmt]) -> Result<(), Ctrl> {
        for s in body {
            self.hoist_stmt(env, s, true)?;
        }
        Ok(())
    }

    fn hoist_stmt(&mut self, env: EnvId, s: &Stmt, top: bool) -> Result<(), Ctrl> {
        match s {
            Stmt::Function(f) if top => {
                let v = self.closure(f, env, None)?;
                if let Some(name) = &f.name {
                    self.declare(env, name, v, false);
                }
            }
            Stmt::Var(VarKind::Var, decls) => {
                for (p, _) in decls {
                    let mut names = Vec::new();
                    pattern_names(p, &mut names);
                    for n in names {
                        let exists = if env == self.global_env {
                            self.get_own(self.global, &n).is_some()
                        } else {
                            self.envs[env as usize].vars.iter().any(|b| b.name == n)
                        };
                        if !exists {
                            self.declare(env, &n, Value::Undefined, false);
                        }
                    }
                }
            }
            Stmt::If(_, a, b) => {
                self.hoist_stmt(env, a, false)?;
                if let Some(b) = b {
                    self.hoist_stmt(env, b, false)?;
                }
            }
            Stmt::Block(body) | Stmt::Try(body, None, None) => {
                for s in body {
                    self.hoist_stmt(env, s, false)?;
                }
            }
            Stmt::Try(body, handler, fin) => {
                for s in body {
                    self.hoist_stmt(env, s, false)?;
                }
                if let Some((_, h)) = handler {
                    for s in h {
                        self.hoist_stmt(env, s, false)?;
                    }
                }
                if let Some(f) = fin {
                    for s in f {
                        self.hoist_stmt(env, s, false)?;
                    }
                }
            }
            Stmt::While(_, b) | Stmt::DoWhile(b, _) | Stmt::Labeled(_, b) => {
                self.hoist_stmt(env, b, false)?
            }
            Stmt::For(init, _, _, b) => {
                if let Some(i) = init {
                    self.hoist_stmt(env, i, false)?;
                }
                self.hoist_stmt(env, b, false)?;
            }
            Stmt::ForIn(Some(VarKind::Var), p, _, b, _) => {
                let mut names = Vec::new();
                pattern_names(p, &mut names);
                for n in names {
                    self.declare(env, &n, Value::Undefined, false);
                }
                self.hoist_stmt(env, b, false)?;
            }
            Stmt::Switch(_, cases) => {
                for (_, body) in cases {
                    for s in body {
                        self.hoist_stmt(env, s, false)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn closure(&mut self, f: &Arc<Func>, env: EnvId, this: Option<Value>) -> Result<Value, Ctrl> {
        // The scope must outlive this call now.
        self.pinned = self.pinned.max(env as usize + 1);
        let p = self.protos.function;
        let id = self.alloc(Some(p), Kind::Function(f.clone(), env, this))?;
        if !f.arrow {
            let o = self.protos.object;
            let proto = self.alloc(Some(o), Kind::Plain)?;
            self.set_own(proto, "constructor", Value::Obj(id));
            self.set_own(id, "prototype", Value::Obj(proto));
        }
        Ok(Value::Obj(id))
    }

    /// Run statements in a new block scope.
    fn block(&mut self, host: &mut dyn Host, env: EnvId, body: &[Stmt]) -> Result<(), Ctrl> {
        let mark = self.envs.len();
        let inner = self.new_env(env, None)?;
        for s in body {
            if let Stmt::Function(f) = s {
                let v = self.closure(f, inner, None)?;
                if let Some(name) = &f.name {
                    self.declare(inner, name, v, false);
                }
            }
        }
        let r = body.iter().try_for_each(|s| self.exec(host, inner, s));
        self.release_envs(mark);
        r
    }

    fn exec(&mut self, host: &mut dyn Host, env: EnvId, s: &Stmt) -> Result<(), Ctrl> {
        self.step()?;
        match s {
            Stmt::Expr(e) => {
                self.eval(host, env, e)?;
            }
            Stmt::Var(kind, decls) => {
                for (pattern, init) in decls {
                    let value = match init {
                        Some(e) => self.eval_named(host, env, e, pattern)?,
                        None if *kind == VarKind::Var => continue,
                        None => Value::Undefined,
                    };
                    self.bind(host, env, pattern, value, Some(*kind))?;
                }
            }
            Stmt::Function(_) => {}
            Stmt::Return(e) => {
                let v = match e {
                    Some(e) => self.eval(host, env, e)?,
                    None => Value::Undefined,
                };
                return Err(Ctrl::Return(v));
            }
            Stmt::If(test, then, otherwise) => {
                let t = self.eval(host, env, test)?;
                if self.truthy(&t) {
                    self.exec(host, env, then)?;
                } else if let Some(o) = otherwise {
                    self.exec(host, env, o)?;
                }
            }
            Stmt::Block(body) => self.block(host, env, body)?,
            Stmt::While(test, body) => {
                let label = self.pending_label.take();
                loop {
                    let t = self.eval(host, env, test)?;
                    if !self.truthy(&t) {
                        break;
                    }
                    match self.exec(host, env, body) {
                        Err(Ctrl::Break(None)) => break,
                        Err(Ctrl::Continue(l)) if l.is_none() || l == label => continue,
                        other => other?,
                    }
                }
            }
            Stmt::DoWhile(body, test) => {
                let label = self.pending_label.take();
                loop {
                    match self.exec(host, env, body) {
                        Err(Ctrl::Break(None)) => break,
                        Err(Ctrl::Continue(l)) if l.is_none() || l == label => {}
                        other => other?,
                    }
                    let t = self.eval(host, env, test)?;
                    if !self.truthy(&t) {
                        break;
                    }
                }
            }
            Stmt::For(init, test, update, body) => {
                let label = self.pending_label.take();
                let mark = self.envs.len();
                let scope = self.new_env(env, None)?;
                if let Some(i) = init {
                    self.exec(host, scope, i)?;
                }
                // `let` loop variables get a fresh binding each pass, so closures made in
                // the body see that pass's value.
                let per_pass = matches!(
                    init.as_deref(),
                    Some(Stmt::Var(VarKind::Let | VarKind::Const, _))
                );
                let r = (|| -> Result<(), Ctrl> {
                    let mut current = scope;
                    loop {
                        if let Some(t) = test {
                            let t = self.eval(host, current, t)?;
                            if !self.truthy(&t) {
                                return Ok(());
                            }
                        }
                        match self.exec(host, current, body) {
                            Err(Ctrl::Break(None)) => return Ok(()),
                            Err(Ctrl::Continue(l)) if l.is_none() || l == label => {}
                            other => other?,
                        }
                        if per_pass && self.pinned > current as usize {
                            let vars: Vec<Binding> = self.envs[current as usize]
                                .vars
                                .iter()
                                .map(|b| Binding {
                                    name: b.name.clone(),
                                    value: b.value.clone(),
                                    constant: b.constant,
                                })
                                .collect();
                            let next = self.new_env(env, None)?;
                            self.envs[next as usize].vars = vars;
                            current = next;
                        }
                        if let Some(u) = update {
                            self.eval(host, current, u)?;
                        }
                    }
                })();
                self.release_envs(mark);
                r?;
            }
            Stmt::ForIn(kind, pattern, object, body, of) => {
                let label = self.pending_label.take();
                let o = self.eval(host, env, object)?;
                let items: Vec<Value> = if *of {
                    self.iterate(host, &o)?
                } else {
                    let mut keys = Vec::new();
                    let mut at = if o.is_nullish() {
                        None
                    } else {
                        Some(o.clone())
                    };
                    // Own keys, then inherited ones, as for-in does (prototypes of builtins
                    // hold only methods, which are not enumerable there).
                    while let Some(v) = at {
                        for k in self.keys(&v) {
                            if !keys
                                .iter()
                                .any(|x: &Value| matches!(x, Value::Str(s) if *s == k))
                            {
                                keys.push(Value::Str(k));
                            }
                        }
                        at = match v {
                            Value::Obj(id) => self.objects[id as usize]
                                .proto
                                .filter(|p| !self.is_builtin_proto(*p))
                                .map(Value::Obj),
                            _ => None,
                        };
                    }
                    keys
                };
                for item in items {
                    let mark = self.envs.len();
                    let scope = self.new_env(env, None)?;
                    let k = kind.unwrap_or(VarKind::Var);
                    if kind.is_some() && k != VarKind::Var {
                        self.bind(host, scope, pattern, item, Some(k))?;
                    } else {
                        self.bind(host, env, pattern, item, None)?;
                    }
                    let r = self.exec(host, scope, body);
                    self.release_envs(mark);
                    match r {
                        Err(Ctrl::Break(None)) => break,
                        Err(Ctrl::Continue(l)) if l.is_none() || l == label => {}
                        other => other?,
                    }
                }
            }
            Stmt::Break(l) => return Err(Ctrl::Break(l.clone())),
            Stmt::Continue(l) => return Err(Ctrl::Continue(l.clone())),
            Stmt::Throw(e) => {
                let v = self.eval(host, env, e)?;
                return Err(Ctrl::Throw(v));
            }
            Stmt::Try(body, handler, finalizer) => {
                let mut r = self.block(host, env, body);
                if let (Err(Ctrl::Throw(v)), Some((param, h))) = (&r, handler) {
                    let v = v.clone();
                    let mark = self.envs.len();
                    let scope = self.new_env(env, None)?;
                    if let Some(p) = param {
                        self.bind(host, scope, p, v, Some(VarKind::Let))?;
                    }
                    r = self.block(host, scope, h);
                    self.release_envs(mark);
                }
                if let Some(f) = finalizer {
                    self.block(host, env, f)?;
                }
                r?;
            }
            Stmt::Switch(subject, cases) => {
                let v = self.eval(host, env, subject)?;
                let mut start = None;
                for (i, (test, _)) in cases.iter().enumerate() {
                    if let Some(t) = test {
                        let t = self.eval(host, env, t)?;
                        if self.strict_equals(&v, &t) {
                            start = Some(i);
                            break;
                        }
                    }
                }
                let start = start.or_else(|| cases.iter().position(|(t, _)| t.is_none()));
                if let Some(start) = start {
                    let mark = self.envs.len();
                    let scope = self.new_env(env, None)?;
                    let r = (|| {
                        for (_, body) in &cases[start..] {
                            for s in body {
                                self.exec(host, scope, s)?;
                            }
                        }
                        Ok(())
                    })();
                    self.release_envs(mark);
                    match r {
                        Err(Ctrl::Break(None)) => {}
                        other => other?,
                    }
                }
            }
            Stmt::Labeled(label, body) => {
                let r = {
                    if matches!(
                        **body,
                        Stmt::While(..) | Stmt::DoWhile(..) | Stmt::For(..) | Stmt::ForIn(..)
                    ) {
                        self.pending_label = Some(label.clone());
                    }
                    self.exec(host, env, body)
                };
                match r {
                    Err(Ctrl::Break(Some(l))) if l == *label => {}
                    Err(Ctrl::Continue(Some(l))) if l == *label => {}
                    other => other?,
                }
            }
            Stmt::Empty => {}
        }
        Ok(())
    }

    fn is_builtin_proto(&self, o: ObjId) -> bool {
        let p = self.protos;
        [
            p.object,
            p.function,
            p.array,
            p.string,
            p.number,
            p.boolean,
            p.error,
            p.element,
            p.class_list,
            p.style,
            p.event,
        ]
        .contains(&o)
    }

    /// Evaluate an initializer, naming an anonymous function after its variable.
    fn eval_named(
        &mut self,
        host: &mut dyn Host,
        env: EnvId,
        e: &Expr,
        target: &Pattern,
    ) -> Result<Value, Ctrl> {
        let v = self.eval(host, env, e)?;
        if let (Expr::Function(f), Pattern::Name(n), Value::Obj(o)) = (e, target, &v) {
            if f.name.is_none() {
                self.set_own(*o, "name", Value::Str(n.clone()));
            }
        }
        Ok(v)
    }

    /// Bind `value` to `pattern`: declare in `env` with `kind`, or assign when `None`.
    fn bind(
        &mut self,
        host: &mut dyn Host,
        env: EnvId,
        pattern: &Pattern,
        value: Value,
        kind: Option<VarKind>,
    ) -> Result<(), Ctrl> {
        match pattern {
            Pattern::Name(n) => match kind {
                Some(k) => {
                    let target = if k == VarKind::Var {
                        self.var_scope(env)
                    } else {
                        env
                    };
                    self.declare(target, n, value, k == VarKind::Const);
                    Ok(())
                }
                None => self.assign_name(host, env, n, value),
            },
            Pattern::Object(props, rest) => {
                if value.is_nullish() {
                    return self.type_error("cannot destructure undefined or null");
                }
                let mut used = Vec::new();
                for (key, p, default) in props {
                    let k = self.eval(host, env, key)?;
                    let k = self.property_key(host, &k)?;
                    let mut v = self.get_value(host, &value, &k)?;
                    if let (Value::Undefined, Some(d)) = (&v, default) {
                        v = self.eval(host, env, d)?;
                    }
                    used.push(k);
                    self.bind(host, env, p, v, kind)?;
                }
                if let Some(r) = rest {
                    let o = self.new_object()?;
                    for k in self.keys(&value) {
                        if !used.contains(&k) {
                            let v = self.get_value(host, &value, &k)?;
                            self.put(host, &o, &k, v)?;
                        }
                    }
                    self.bind(host, env, &Pattern::Name(r.clone()), o, kind)?;
                }
                Ok(())
            }
            Pattern::Array(items, rest) => {
                let all = self.iterate(host, &value)?;
                for (i, item) in items.iter().enumerate() {
                    if let Some((p, default)) = item {
                        let mut v = all.get(i).cloned().unwrap_or(Value::Undefined);
                        if let (Value::Undefined, Some(d)) = (&v, default) {
                            v = self.eval(host, env, d)?;
                        }
                        self.bind(host, env, p, v, kind)?;
                    }
                }
                if let Some(r) = rest {
                    let tail = all
                        .get(items.len()..)
                        .map(<[Value]>::to_vec)
                        .unwrap_or_default();
                    let arr = self.new_array(tail)?;
                    self.bind(host, env, &Pattern::Name(r.clone()), arr, kind)?;
                }
                Ok(())
            }
        }
    }

    /// The nearest function (or global) scope, where `var` lives.
    fn var_scope(&self, env: EnvId) -> EnvId {
        let mut e = env;
        loop {
            let scope = &self.envs[e as usize];
            if scope.this.is_some() || scope.parent.is_none() {
                return e;
            }
            e = scope.parent.unwrap_or(self.global_env);
        }
    }

    fn eval_args(
        &mut self,
        host: &mut dyn Host,
        env: EnvId,
        args: &[Elem],
    ) -> Result<Vec<Value>, Ctrl> {
        let mut out = Vec::with_capacity(args.len());
        for a in args {
            match a {
                Elem::Item(e) => out.push(self.eval(host, env, e)?),
                Elem::Spread(e) => {
                    let v = self.eval(host, env, e)?;
                    out.extend(self.iterate(host, &v)?);
                }
                Elem::Hole => out.push(Value::Undefined),
            }
        }
        Ok(out)
    }

    pub fn eval(&mut self, host: &mut dyn Host, env: EnvId, e: &Expr) -> Result<Value, Ctrl> {
        self.step()?;
        Ok(match e {
            Expr::Num(n) => Value::Num(*n),
            Expr::Str(s) => Value::Str(s.clone()),
            Expr::Bool(b) => Value::Bool(*b),
            Expr::Null => Value::Null,
            Expr::Template(quasis, exprs) => {
                let mut s = String::new();
                for (i, q) in quasis.iter().enumerate() {
                    s.push_str(q);
                    if let Some(e) = exprs.get(i) {
                        let v = self.eval(host, env, e)?;
                        s.push_str(&self.to_string(host, &v)?);
                    }
                }
                Value::str(&s)
            }
            Expr::Regex(src) => crate::builtins::regex(self, src)?,
            Expr::Ident(n) => self.lookup(host, env, n)?,
            Expr::This => self.this_of(env),
            Expr::Array(items) => {
                let v = self.eval_args(host, env, items)?;
                self.new_array(v)?
            }
            Expr::Object(props) => {
                let o = self.new_object()?;
                for p in props {
                    match p {
                        Prop::Init(k, v) => {
                            let k = self.eval(host, env, k)?;
                            let k = self.property_key(host, &k)?;
                            let v = self.eval(host, env, v)?;
                            self.put(host, &o, &k, v)?;
                        }
                        Prop::Spread(e) => {
                            let src = self.eval(host, env, e)?;
                            for k in self.keys(&src) {
                                let v = self.get_value(host, &src, &k)?;
                                self.put(host, &o, &k, v)?;
                            }
                        }
                    }
                }
                o
            }
            Expr::Function(f) => {
                let this = f.arrow.then(|| self.this_of(env));
                self.closure(f, env, this)?
            }
            Expr::Unary(op, operand) => {
                if *op == "typeof" {
                    if let Expr::Ident(n) = &**operand {
                        return Ok(match self.lookup(host, env, n) {
                            Ok(v) => Value::str(self.type_of(&v)),
                            Err(Ctrl::Throw(_)) => Value::str("undefined"),
                            Err(e) => return Err(e),
                        });
                    }
                }
                if *op == "delete" {
                    if let Expr::Member(o, k, _) = &**operand {
                        let o = self.eval(host, env, o)?;
                        let k = self.eval(host, env, k)?;
                        let k = self.property_key(host, &k)?;
                        if let Value::Obj(id) = o {
                            self.objects[id as usize].props.retain(|(key, _)| *key != k);
                        }
                    }
                    return Ok(Value::Bool(true));
                }
                let v = self.eval(host, env, operand)?;
                match *op {
                    "!" => Value::Bool(!self.truthy(&v)),
                    "-" => Value::Num(-self.to_number(host, &v)?),
                    "+" => Value::Num(self.to_number(host, &v)?),
                    "~" => Value::Num(f64::from(!to_int32(self.to_number(host, &v)?))),
                    "typeof" => Value::str(self.type_of(&v)),
                    _ => Value::Undefined,
                }
            }
            Expr::Update(op, prefix, target) => {
                let old = self.eval(host, env, target)?;
                let old = self.to_number(host, &old)?;
                let new = if *op == "++" { old + 1.0 } else { old - 1.0 };
                self.assign_to(host, env, target, Value::Num(new))?;
                Value::Num(if *prefix { new } else { old })
            }
            Expr::Binary(op, a, b) => {
                let a = self.eval(host, env, a)?;
                let b = self.eval(host, env, b)?;
                self.binary(host, op, a, b)?
            }
            Expr::Logical(op, a, b) => {
                let a = self.eval(host, env, a)?;
                let short = match *op {
                    "&&" => !self.truthy(&a),
                    "||" => self.truthy(&a),
                    _ => !a.is_nullish(),
                };
                if short {
                    a
                } else {
                    self.eval(host, env, b)?
                }
            }
            Expr::Cond(t, a, b) => {
                let t = self.eval(host, env, t)?;
                if self.truthy(&t) {
                    self.eval(host, env, a)?
                } else {
                    self.eval(host, env, b)?
                }
            }
            Expr::Assign(op, target, value) => {
                let v = match *op {
                    "=" => {
                        let v = self.eval(host, env, value)?;
                        if let (Expr::Function(f), Expr::Ident(n), Value::Obj(o)) =
                            (&**value, &**target, &v)
                        {
                            if f.name.is_none() && self.get_own(*o, "name").is_none() {
                                self.set_own(*o, "name", Value::Str(n.clone()));
                            }
                        }
                        v
                    }
                    "&&=" | "||=" | "??=" => {
                        let cur = self.eval(host, env, target)?;
                        let keep = match *op {
                            "&&=" => !self.truthy(&cur),
                            "||=" => self.truthy(&cur),
                            _ => !cur.is_nullish(),
                        };
                        if keep {
                            return Ok(cur);
                        }
                        self.eval(host, env, value)?
                    }
                    _ => {
                        let cur = self.eval(host, env, target)?;
                        let rhs = self.eval(host, env, value)?;
                        self.binary(host, &op[..op.len() - 1], cur, rhs)?
                    }
                };
                self.assign_to(host, env, target, v.clone())?;
                v
            }
            Expr::Member(o, k, optional) => {
                let o = self.eval(host, env, o)?;
                if *optional && o.is_nullish() {
                    return Ok(Value::Undefined);
                }
                let k = self.eval(host, env, k)?;
                let k = self.property_key(host, &k)?;
                self.get_value(host, &o, &k)?
            }
            Expr::Call(callee, args, optional) => {
                let (f, this) = match &**callee {
                    Expr::Member(o, k, opt) => {
                        let o = self.eval(host, env, o)?;
                        if *opt && o.is_nullish() {
                            return Ok(Value::Undefined);
                        }
                        let k = self.eval(host, env, k)?;
                        let k = self.property_key(host, &k)?;
                        let f = self.get_value(host, &o, &k)?;
                        if !(self.is_callable(&f) || *optional && f.is_nullish()) {
                            return self.type_error(&format!("{k} is not a function"));
                        }
                        (f, o)
                    }
                    other => {
                        let f = self.eval(host, env, other)?;
                        (f, Value::Undefined)
                    }
                };
                if *optional && f.is_nullish() {
                    return Ok(Value::Undefined);
                }
                let args = self.eval_args(host, env, args)?;
                self.call(host, &f, this, &args)?
            }
            Expr::New(callee, args) => {
                let f = self.eval(host, env, callee)?;
                let args = self.eval_args(host, env, args)?;
                self.construct(host, &f, &args)?
            }
            Expr::Seq(items) => {
                let mut last = Value::Undefined;
                for e in items {
                    last = self.eval(host, env, e)?;
                }
                last
            }
        })
    }

    fn assign_to(
        &mut self,
        host: &mut dyn Host,
        env: EnvId,
        target: &Expr,
        v: Value,
    ) -> Result<(), Ctrl> {
        match target {
            Expr::Ident(n) => self.assign_name(host, env, n, v),
            Expr::Member(o, k, _) => {
                let o = self.eval(host, env, o)?;
                let k = self.eval(host, env, k)?;
                let k = self.property_key(host, &k)?;
                self.put(host, &o, &k, v)
            }
            _ => self.type_error("invalid assignment target"),
        }
    }

    pub fn binary(
        &mut self,
        host: &mut dyn Host,
        op: &str,
        a: Value,
        b: Value,
    ) -> Result<Value, Ctrl> {
        let num = |it: &mut Interp, host: &mut dyn Host, v: &Value| it.to_number(host, v);
        Ok(match op {
            "+" => {
                let a = self.to_primitive(host, &a)?;
                let b = self.to_primitive(host, &b)?;
                if matches!(a, Value::Str(_)) || matches!(b, Value::Str(_)) {
                    let x = self.to_string(host, &a)?;
                    let y = self.to_string(host, &b)?;
                    if x.len() + y.len() > STRING_LIMIT * 4 {
                        return Err(Ctrl::Abort("a string grew too large".into()));
                    }
                    let mut s = String::with_capacity(x.len() + y.len());
                    s.push_str(&x);
                    s.push_str(&y);
                    Value::str(&s)
                } else {
                    Value::Num(num(self, host, &a)? + num(self, host, &b)?)
                }
            }
            "-" => Value::Num(num(self, host, &a)? - num(self, host, &b)?),
            "*" => Value::Num(num(self, host, &a)? * num(self, host, &b)?),
            "/" => Value::Num(num(self, host, &a)? / num(self, host, &b)?),
            "%" => Value::Num(num(self, host, &a)? % num(self, host, &b)?),
            "**" => Value::Num(num(self, host, &a)?.powf(num(self, host, &b)?)),
            "==" => Value::Bool(self.loose_equals(host, &a, &b)?),
            "!=" => Value::Bool(!self.loose_equals(host, &a, &b)?),
            "===" => Value::Bool(self.strict_equals(&a, &b)),
            "!==" => Value::Bool(!self.strict_equals(&a, &b)),
            "<" | ">" | "<=" | ">=" => {
                let a = self.to_primitive(host, &a)?;
                let b = self.to_primitive(host, &b)?;
                if let (Value::Str(x), Value::Str(y)) = (&a, &b) {
                    let o = x.cmp(y);
                    return Ok(Value::Bool(match op {
                        "<" => o.is_lt(),
                        ">" => o.is_gt(),
                        "<=" => o.is_le(),
                        _ => o.is_ge(),
                    }));
                }
                let (x, y) = (num(self, host, &a)?, num(self, host, &b)?);
                Value::Bool(match op {
                    "<" => x < y,
                    ">" => x > y,
                    "<=" => x <= y,
                    _ => x >= y,
                })
            }
            "&" => Value::Num(f64::from(
                to_int32(num(self, host, &a)?) & to_int32(num(self, host, &b)?),
            )),
            "|" => Value::Num(f64::from(
                to_int32(num(self, host, &a)?) | to_int32(num(self, host, &b)?),
            )),
            "^" => Value::Num(f64::from(
                to_int32(num(self, host, &a)?) ^ to_int32(num(self, host, &b)?),
            )),
            "<<" => Value::Num(f64::from(
                to_int32(num(self, host, &a)?)
                    .wrapping_shl(to_int32(num(self, host, &b)?) as u32 & 31),
            )),
            ">>" => Value::Num(f64::from(
                to_int32(num(self, host, &a)?)
                    .wrapping_shr(to_int32(num(self, host, &b)?) as u32 & 31),
            )),
            ">>>" => Value::Num(f64::from(
                (to_int32(num(self, host, &a)?) as u32)
                    .wrapping_shr(to_int32(num(self, host, &b)?) as u32 & 31),
            )),
            "instanceof" => {
                let proto = self.get_value(host, &b, "prototype")?;
                let mut at = match a {
                    Value::Obj(o) => self.objects[o as usize].proto,
                    _ => None,
                };
                let mut found = false;
                while let Some(p) = at {
                    if matches!(proto, Value::Obj(q) if q == p) {
                        found = true;
                        break;
                    }
                    at = self.objects[p as usize].proto;
                }
                Value::Bool(found)
            }
            "in" => {
                let k = self.property_key(host, &a)?;
                if !matches!(b, Value::Obj(_)) {
                    return self.type_error("'in' needs an object");
                }
                Value::Bool(self.get(host, &b, &k)?.is_some())
            }
            _ => Value::Undefined,
        })
    }

    pub fn call(
        &mut self,
        host: &mut dyn Host,
        f: &Value,
        this: Value,
        args: &[Value],
    ) -> Result<Value, Ctrl> {
        let Value::Obj(id) = f else {
            return self.type_error("not a function");
        };
        if self.depth >= DEPTH_LIMIT {
            return Err(self.throw_error("RangeError", "maximum call stack size exceeded"));
        }
        let (func, scope, arrow_this) = match &self.objects[*id as usize].kind {
            Kind::Native(n) => {
                let n = *n;
                self.depth += 1;
                let r = n(self, host, this, args);
                self.depth -= 1;
                return r;
            }
            Kind::Bound(target, bound_this, bound_args) => {
                let (target, bound_this) = (*target, bound_this.clone());
                let mut all = bound_args.clone();
                all.extend_from_slice(args);
                return self.call(host, &Value::Obj(target), bound_this, &all);
            }
            Kind::Function(func, scope, arrow_this) => (func.clone(), *scope, arrow_this.clone()),
            _ => return self.type_error("not a function"),
        };
        let mark = self.envs.len();
        // Arrows keep the `this` they were made with; their scope still holds their `var`s.
        let this = if func.arrow {
            Some(arrow_this.unwrap_or(Value::Undefined))
        } else {
            Some(match this {
                Value::Undefined | Value::Null => Value::Obj(self.global),
                t => t,
            })
        };
        let env = self.new_env(scope, this)?;
        self.depth += 1;
        let r = self.call_body(host, env, &func, args, *id);
        self.depth -= 1;
        self.release_envs(mark);
        match r {
            Ok(()) => Ok(Value::Undefined),
            Err(Ctrl::Return(v)) => Ok(v),
            Err(Ctrl::Break(_) | Ctrl::Continue(_)) => Ok(Value::Undefined),
            Err(e) => Err(e),
        }
    }

    fn call_body(
        &mut self,
        host: &mut dyn Host,
        env: EnvId,
        func: &Arc<Func>,
        args: &[Value],
        callee: ObjId,
    ) -> Result<(), Ctrl> {
        if !func.arrow {
            let a = self.new_array(args.to_vec())?;
            self.declare(env, &Name::from("arguments"), a, false);
            // A named function expression can call itself by name.
            if let Some(n) = &func.name {
                if !self.envs[env as usize].vars.iter().any(|b| b.name == *n) {
                    self.declare(env, n, Value::Obj(callee), false);
                }
            }
        }
        for (
            i,
            Param {
                pattern,
                default,
                rest,
            },
        ) in func.params.iter().enumerate()
        {
            let v = if *rest {
                let tail = args.get(i..).map(<[Value]>::to_vec).unwrap_or_default();
                self.new_array(tail)?
            } else {
                let mut v = args.get(i).cloned().unwrap_or(Value::Undefined);
                if let (Value::Undefined, Some(d)) = (&v, default) {
                    v = self.eval(host, env, d)?;
                }
                v
            };
            self.bind(host, env, pattern, v, Some(VarKind::Let))?;
        }
        self.hoist(env, &func.body)?;
        for s in &func.body {
            self.exec(host, env, s)?;
        }
        Ok(())
    }

    pub fn construct(
        &mut self,
        host: &mut dyn Host,
        f: &Value,
        args: &[Value],
    ) -> Result<Value, Ctrl> {
        if !self.is_callable(f) {
            return self.type_error("not a constructor");
        }
        if let Value::Obj(id) = f {
            if let Some(Value::Obj(c)) = self.get_own(*id, "__construct") {
                return self.call(host, &Value::Obj(c), Value::Undefined, args);
            }
        }
        let proto = match self.get_value(host, f, "prototype")? {
            Value::Obj(p) => p,
            _ => self.protos.object,
        };
        let obj = Value::Obj(self.alloc(Some(proto), Kind::Plain)?);
        let r = self.call(host, f, obj.clone(), args)?;
        Ok(if matches!(r, Value::Obj(_)) { r } else { obj })
    }

    /// Run `source` as a function body with `this` and one argument `event`, for handlers
    /// written in attributes (`onclick="…"`).
    pub fn compile_handler(&mut self, source: &str) -> Result<Value, String> {
        let program = crate::parser::parse(source)?;
        let func = Arc::new(Func {
            name: None,
            params: vec![Param {
                pattern: Pattern::Name("event".into()),
                default: None,
                rest: false,
            }],
            body: program,
            arrow: false,
        });
        let env = self.global_env;
        self.closure(&func, env, None)
            .map_err(|_| "out of memory".to_string())
    }

    /// Describe a thrown value for the console.
    pub fn describe(&mut self, host: &mut dyn Host, v: &Value) -> String {
        if let Value::Obj(o) = v {
            if matches!(self.objects[*o as usize].kind, Kind::Error) {
                let name = self
                    .get_own(*o, "name")
                    .map(|n| {
                        self.to_string(host, &n)
                            .map(|s| s.to_string())
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                let msg = self
                    .get_own(*o, "message")
                    .map(|n| {
                        self.to_string(host, &n)
                            .map(|s| s.to_string())
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                return format!("{name}: {msg}");
            }
        }
        crate::builtins::inspect(self, host, v, 0)
    }
}

impl Default for Interp {
    fn default() -> Self {
        Interp::new()
    }
}

fn pattern_names(p: &Pattern, out: &mut Vec<Name>) {
    match p {
        Pattern::Name(n) => out.push(n.clone()),
        Pattern::Object(props, rest) => {
            for (_, p, _) in props {
                pattern_names(p, out);
            }
            out.extend(rest.clone());
        }
        Pattern::Array(items, rest) => {
            for (p, _) in items.iter().flatten() {
                pattern_names(p, out);
            }
            out.extend(rest.clone());
        }
    }
}
