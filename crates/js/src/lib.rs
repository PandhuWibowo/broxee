//! JavaScript for Broxee, written here: a lexer, a parser and a tree-walking interpreter
//! with the builtins and DOM bindings a page's scripts need first (`querySelector`,
//! `textContent`, `classList`, `style`, `addEventListener`, timers, `console`).
//!
//! The interpreter knows nothing of Broxee's DOM: it reaches the page through `Host`, which
//! the browser implements. One `Runtime` belongs to one page and is dropped with it; its heap
//! has a ceiling and every entry (a script, a handler, a timer) a step limit, so a page can
//! neither hang the browser nor spend its memory budget.

mod builtins;
mod interp;
mod lexer;
mod parser;
mod regex;

pub use interp::{number_to_string, Ctrl, Interp, Target, Value};

/// The stack a thread running scripts needs: the interpreter recurses for calls and nested
/// expressions (up to its own limits). It is reserved, not committed: only the pages touched
/// count against memory.
pub const STACK_SIZE: usize = 64 << 20;

/// What scripts can see and change of the page. Nodes are the DOM's node ids; node 0 is the
/// document.
pub trait Host {
    /// Elements under `root` (the document when `None`) matching `selector`, in order.
    fn select(&mut self, root: Option<u32>, selector: &str) -> Vec<u32>;
    fn matches(&mut self, node: u32, selector: &str) -> bool;
    /// Lowercase tag name; `None` for text and other non-elements.
    fn tag(&self, node: u32) -> Option<String>;
    fn is_text(&self, node: u32) -> bool;
    fn text(&self, node: u32) -> String;
    /// Replace the node's children with one text node (or set a text node's text).
    fn set_text(&mut self, node: u32, text: &str);
    /// Replace the node's children with parsed HTML.
    fn set_html(&mut self, node: u32, html: &str);
    fn attr(&self, node: u32, name: &str) -> Option<String>;
    fn set_attr(&mut self, node: u32, name: &str, value: &str);
    fn remove_attr(&mut self, node: u32, name: &str);
    fn parent(&self, node: u32) -> Option<u32>;
    fn children(&self, node: u32) -> Vec<u32>;
    fn create_element(&mut self, tag: &str) -> u32;
    fn create_text(&mut self, text: &str) -> u32;
    fn append(&mut self, parent: u32, child: u32) -> bool;
    fn insert_before(&mut self, parent: u32, child: u32, before: u32) -> bool;
    fn remove(&mut self, node: u32);
    /// A form control's value and checked state, if `node` is one.
    fn value(&self, node: u32) -> Option<String>;
    fn set_value(&mut self, node: u32, value: &str);
    fn checked(&self, node: u32) -> Option<bool>;
    fn set_checked(&mut self, node: u32, checked: bool);
    fn url(&self) -> String;
    fn log(&mut self, line: &str);
}

/// The scripts of one page.
pub struct Runtime {
    interp: Interp,
    /// Lines written to the console, and errors.
    pub console: Vec<String>,
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime::new()
    }
}

impl Runtime {
    pub fn new() -> Runtime {
        Runtime {
            interp: Interp::new(),
            console: Vec::new(),
        }
    }

    /// Whether the page's scripts were stopped, and why.
    pub fn stopped(&self) -> Option<&str> {
        self.interp.stopped.as_deref()
    }

    /// Advance the page's clock: milliseconds since the page started, and the wall clock.
    pub fn set_time(&mut self, now_ms: f64, epoch_ms: f64) {
        self.interp.now = now_ms;
        self.interp.epoch_ms = epoch_ms;
    }

    fn finish(&mut self, host: &mut dyn Host, r: Result<Value, Ctrl>, what: &str) -> Value {
        self.interp.steps = 0;
        match r {
            Ok(v) => v,
            Err(Ctrl::Throw(v)) => {
                let line = format!("Uncaught {} ({what})", self.interp.describe(host, &v));
                host.log(&line);
                self.console.push(line);
                Value::Undefined
            }
            Err(Ctrl::Abort(why)) => {
                let line = format!("scripts stopped: {why}");
                host.log(&line);
                self.console.push(line);
                self.interp.stopped = Some(why);
                self.interp.timers.clear();
                Value::Undefined
            }
            Err(_) => Value::Undefined,
        }
    }

    /// Parse and run a script. Syntax Broxee does not support skips the script, as an error
    /// would in a browser.
    pub fn run(&mut self, host: &mut dyn Host, source: &str, name: &str) {
        if self.interp.stopped.is_some() {
            return;
        }
        let program = match parser::parse(source) {
            Ok(p) => p,
            Err(e) => {
                let line = format!("SyntaxError in {name}: {e}");
                host.log(&line);
                self.console.push(line);
                return;
            }
        };
        let r = self
            .interp
            .run_program(host, &program)
            .map(|_| Value::Undefined);
        self.finish(host, r, name);
    }

    /// Tell listeners the document is ready (`DOMContentLoaded`, then `load`).
    pub fn loaded(&mut self, host: &mut dyn Host) {
        self.dispatch(host, 0, "DOMContentLoaded");
        self.dispatch_window(host, "load");
    }

    /// Whether any listener, or an `onclick` attribute, could react to a click on `node`.
    pub fn listens(&self, host: &dyn Host, node: u32, kind: &str) -> bool {
        if self.interp.stopped.is_some() {
            return false;
        }
        let attr = format!("on{kind}");
        let mut n = Some(node);
        while let Some(id) = n {
            let property = self
                .interp
                .elements
                .get(&id)
                .and_then(|&o| self.interp.get_own(o, &format!("__{attr}")))
                .is_some_and(|f| self.interp.is_callable(&f));
            if property
                || host.attr(id, &attr).is_some()
                || self
                    .interp
                    .listeners
                    .iter()
                    .any(|(t, k, _)| *t == Target::Node(id) && &**k == kind)
            {
                return true;
            }
            n = host.parent(id);
        }
        self.interp
            .listeners
            .iter()
            .any(|(t, k, _)| *t == Target::Window && &**k == kind)
    }

    /// Fire an event of `kind` at `node`, bubbling to the document and the window. Returns
    /// whether a listener prevented the default action (following a link, submitting).
    pub fn dispatch(&mut self, host: &mut dyn Host, node: u32, kind: &str) -> bool {
        if self.interp.stopped.is_some() {
            return false;
        }
        let r = builtins::dispatch(&mut self.interp, host, node, kind);
        match r {
            Ok(prevented) => {
                self.interp.steps = 0;
                prevented
            }
            Err(e) => {
                self.finish(host, Err(e), kind);
                false
            }
        }
    }

    fn dispatch_window(&mut self, host: &mut dyn Host, kind: &str) {
        if self.interp.stopped.is_some() {
            return;
        }
        let r = builtins::dispatch_window(&mut self.interp, host, kind);
        if let Err(e) = r {
            self.finish(host, Err(e), kind);
        }
        self.interp.steps = 0;
    }

    /// When the next timer is due, in page milliseconds.
    pub fn next_timer(&self) -> Option<f64> {
        if self.interp.stopped.is_some() {
            return None;
        }
        self.interp.timers.iter().map(|t| t.due).reduce(f64::min)
    }

    /// Run the timers due at the current time (see `set_time`). Returns how many ran.
    pub fn run_timers(&mut self, host: &mut dyn Host) -> usize {
        let mut ran = 0;
        // Bounded, so timers that keep scheduling zero-delay timers cannot spin forever.
        for _ in 0..100 {
            if self.interp.stopped.is_some() {
                break;
            }
            let now = self.interp.now;
            let Some(i) = self.interp.timers.iter().position(|t| t.due <= now) else {
                break;
            };
            let (callback, args, id, every) = {
                let t = &self.interp.timers[i];
                (t.callback.clone(), t.args.clone(), t.id, t.every)
            };
            match every {
                Some(ms) => self.interp.timers[i].due = now + ms.max(4.0),
                None => {
                    self.interp.timers.remove(i);
                }
            }
            let r = if let Value::Str(code) = &callback {
                let code = code.to_string();
                self.run(host, &code, "timer");
                Ok(Value::Undefined)
            } else {
                self.interp.call(host, &callback, Value::Undefined, &args)
            };
            self.finish(host, r, &format!("timer {id}"));
            ran += 1;
        }
        ran
    }

    /// Evaluate an expression and describe its value, for tests and debugging.
    pub fn eval_to_string(&mut self, host: &mut dyn Host, source: &str) -> String {
        let program = match parser::parse(source) {
            Ok(p) => p,
            Err(e) => return format!("SyntaxError: {e}"),
        };
        let last = match program.last() {
            Some(parser::Stmt::Expr(_)) => program.len() - 1,
            _ => program.len(),
        };
        if let Err(e) = self.interp.run_program(host, &program[..last]) {
            return match e {
                Ctrl::Throw(v) => format!("Uncaught {}", self.interp.describe(host, &v)),
                other => format!("{other:?}"),
            };
        }
        let env = self.interp.global_env;
        let r = match program.get(last) {
            Some(parser::Stmt::Expr(e)) => self.interp.eval(host, env, e),
            _ => Ok(Value::Undefined),
        };
        self.interp.steps = 0;
        match r {
            Ok(v) => builtins::inspect(&mut self.interp, host, &v, 0),
            Err(Ctrl::Throw(v)) => format!("Uncaught {}", self.interp.describe(host, &v)),
            Err(other) => format!("{other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host with no document, for language tests.
    #[derive(Default)]
    struct Bare {
        log: Vec<String>,
    }

    impl Host for Bare {
        fn select(&mut self, _: Option<u32>, _: &str) -> Vec<u32> {
            Vec::new()
        }
        fn matches(&mut self, _: u32, _: &str) -> bool {
            false
        }
        fn tag(&self, _: u32) -> Option<String> {
            None
        }
        fn is_text(&self, _: u32) -> bool {
            false
        }
        fn text(&self, _: u32) -> String {
            String::new()
        }
        fn set_text(&mut self, _: u32, _: &str) {}
        fn set_html(&mut self, _: u32, _: &str) {}
        fn attr(&self, _: u32, _: &str) -> Option<String> {
            None
        }
        fn set_attr(&mut self, _: u32, _: &str, _: &str) {}
        fn remove_attr(&mut self, _: u32, _: &str) {}
        fn parent(&self, _: u32) -> Option<u32> {
            None
        }
        fn children(&self, _: u32) -> Vec<u32> {
            Vec::new()
        }
        fn create_element(&mut self, _: &str) -> u32 {
            1
        }
        fn create_text(&mut self, _: &str) -> u32 {
            1
        }
        fn append(&mut self, _: u32, _: u32) -> bool {
            true
        }
        fn insert_before(&mut self, _: u32, _: u32, _: u32) -> bool {
            true
        }
        fn remove(&mut self, _: u32) {}
        fn value(&self, _: u32) -> Option<String> {
            None
        }
        fn set_value(&mut self, _: u32, _: &str) {}
        fn checked(&self, _: u32) -> Option<bool> {
            None
        }
        fn set_checked(&mut self, _: u32, _: bool) {}
        fn url(&self) -> String {
            "https://example.org/".into()
        }
        fn log(&mut self, line: &str) {
            self.log.push(line.to_string());
        }
    }

    fn eval(src: &str) -> String {
        Runtime::new().eval_to_string(&mut Bare::default(), src)
    }

    /// Run `f` on a thread with the stack scripts get in the browser.
    fn big_stack(f: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(STACK_SIZE)
            .spawn(f)
            .expect("spawn")
            .join()
            .expect("the test passed");
    }

    #[test]
    fn evaluates_the_language() {
        big_stack(evaluates_the_language_inner);
    }

    fn evaluates_the_language_inner() {
        let cases = [
            ("1 + 2 * 3", "7"),
            ("'a' + 1 + 2", "a12"),
            ("0.1 + 0.2", "0.30000000000000004"),
            ("2 ** 10", "1024"),
            ("7 % 3 + (5 / 2)", "3.5"),
            ("typeof null + typeof undefined + typeof (() => 1)", "objectundefinedfunction"),
            ("null == undefined && '1' == 1 && !('1' === 1)", "true"),
            ("var x = 5; x += 2; x++; x", "8"),
            ("let {a, b: [c, d = 4], ...rest} = {a: 1, b: [3], e: 5, f: 6}; [a, c, d, Object.keys(rest)]", "[1, 3, 4, [\"e\", \"f\"]]"),
            ("function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2) } fib(15)", "610"),
            ("const add = (a, b = 10) => a + b; add(1) + add(1, 2)", "14"),
            ("function f(...xs) { return xs.length + arguments.length } f(1, 2, 3)", "6"),
            ("var fs = []; for (let i = 0; i < 3; i++) fs.push(() => i); fs.map(f => f()).join()", "0,1,2"),
            ("function counter() { let n = 0; return () => ++n } const c = counter(); c(); c(); c()", "3"),
            ("[3, 1, 2].sort().map(x => x * 2).filter(x => x > 2).reduce((a, b) => a + b, 0)", "10"),
            ("[1, [2, 3]].flat().includes(3) && [1,2,3].indexOf(2) === 1", "true"),
            ("'Hello World'.split(' ').map(w => w.toUpperCase()).join('-')", "HELLO-WORLD"),
            ("'  pad '.trim().padStart(5, '*') + 'abc'.slice(-2)", "**padbc"),
            ("'a-b_c d'.replace(/[-_ ]/g, '.')", "a.b.c.d"),
            ("'2024-01-15'.match(/(\\d+)-(\\d+)/).slice(1).join('|')", "2024|01"),
            ("/^\\w+@\\w+\\.com$/.test('me@site.com')", "true"),
            ("'x1y22z333'.replace(/\\d+/g, m => '[' + m.length + ']')", "x[1]y[2]z[3]"),
            ("JSON.stringify({a: [1, 'two', null], b: {c: true}})", "{\"a\":[1,\"two\",null],\"b\":{\"c\":true}}"),
            ("JSON.parse('{\"x\": [1, 2.5, \"s\"]}').x[1]", "2.5"),
            ("var r; try { null.x } catch (e) { r = e instanceof TypeError && e.message.includes('null') } r", "true"),
            ("var r; try { throw new Error('boom') } catch ({message}) { r = message } finally { r += '!' } r", "boom!"),
            ("let s = 0; outer: for (const i of [1,2,3]) { for (const j of [1,2]) { if (j == 2) continue outer; s += i } } s", "6"),
            ("switch (3) { case 1: 'one'; break; case 3: var r = 'three'; break; default: r = 'x' } r", "three"),
            ("var o = {n: 2, m() { return this.n * 3 }}; o.m() + o.m.call({n: 5})", "21"),
            ("function P(x) { this.x = x } P.prototype.get = function () { return this.x }; new P(4).get()", "4"),
            ("const m = new Map([['a', 1]]); m.set('b', 2); [m.get('b'), m.has('a'), new Set([1, 1, 2]).has(2)]", "[2, true, true]"),
            ("`sum ${1 + 1} and ${'x'.repeat(3)}`", "sum 2 and xxx"),
            ("[...'abc', ...[1]].length + Math.max(1, 5, 3) + Math.floor(2.7)", "11"),
            ("parseInt('42px') + parseFloat('1.5e1') + Number('0x10')", "73"),
            ("(255).toString(16) + (3.14159).toFixed(2)", "ff3.14"),
            ("a?.b?.c ?? 'none'", "Uncaught ReferenceError: a is not defined"),
            ("var a = null; a?.b?.c ?? 'none'", "none"),
            ("1e21 + ' ' + 1e-7 + ' ' + (-0) + ' ' + (1/0)", "1e+21 1e-7 0 Infinity"),
            ("encodeURIComponent('a b&c/é')", "a%20b%26c%2F%C3%A9"),
        ];
        for (src, want) in cases {
            assert_eq!(eval(src), want, "{src}");
        }
    }

    #[test]
    fn runs_classes_and_accessors() {
        big_stack(runs_classes_and_accessors_inner);
    }

    fn runs_classes_and_accessors_inner() {
        let cases = [
            ("class A { constructor(x) { this.x = x } double() { return this.x * 2 } } new A(4).double()", "8"),
            ("class A { n = 1; m = this.n + 1; static k = 'K'; static of() { return new this() } } const a = A.of(); [a.n, a.m, A.k, a instanceof A]", "[1, 2, \"K\", true]"),
            ("class A { constructor(x) { this.x = x } hi() { return 'A' + this.x } static make() { return 's' } }\
              class B extends A { y = 'y'; constructor(x) { super(x + 1); this.z = this.y } hi() { return 'B' + super.hi() } }\
              const b = new B(1); [b.hi(), b.z, b instanceof A, B.make(), Object.getPrototypeOf(B) === A]", "[\"BA2\", \"y\", true, \"s\", true]"),
            ("class A { constructor(...xs) { this.n = xs.length } } class B extends A { f = this.n } [new B(1, 2, 3).f, new B().n]", "[3, 0]"),
            ("class Temp { #c = 0; get f() { return this.#c * 9 / 5 + 32 } set f(v) { this.#c = (v - 32) * 5 / 9 } } const t = new Temp(); t.f = 212; [t.f, Math.round(t.#c)]", "[212, 100]"),
            ("class A { static #count = 0; static next() { return ++A.#count } static { A.ready = true } } A.next(); [A.next(), A.ready]", "[2, true]"),
            ("class MyError extends Error { constructor(m) { super(m); this.name = 'MyError' } }\
              var r; try { throw new MyError('bad') } catch (e) { r = [e instanceof MyError, e instanceof Error, e.message, e.name] } r", "[true, true, \"bad\", \"MyError\"]"),
            ("class A {} var r; try { A() } catch (e) { r = e instanceof TypeError } r", "true"),
            ("const C = class Named { who() { return Named.name } }; [new C().who(), C.name]", "[\"Named\", \"Named\"]"),
            ("class A { ['comp' + 'uted']() { return 1 } handle = () => this.v; v = 7 } const a = new A(); const h = a.handle; [a.computed(), h()]", "[1, 7]"),
            ("var o = { _v: 1, get v() { return this._v * 10 }, set v(x) { this._v = x } }; o.v = 3; [o.v, o._v]", "[30, 3]"),
            ("var o = {}; Object.defineProperty(o, 'x', { get() { return 'got' } }); o.x = 'ignored'; o.x", "got"),
            ("var a = true; a?.5:1", "0.5"),
            ("const m = new Map([['a', 1], ['b', 2]]); let s = ''; for (const [k, v] of m) s += k + v; [s, [...new Set([3, 3, 4])]]", "[\"a1b2\", [3, 4]]"),
            ("class R { constructor(n) { this.n = n } [Symbol.iterator]() { let i = 0; const n = this.n; return { next: () => i < n ? { value: i++, done: false } : { done: true } } } } [...new R(3)]", "[0, 1, 2]"),
            ("const f = Object.freeze({}); [Object.isFrozen(f), Object.isFrozen({}), Object.isExtensible({})]", "[true, false, true]"),
            ("var o = { get g() { return 1 }, v: 2 }; const d = Object.getOwnPropertyDescriptor(o, 'g'); [typeof d.get, Object.getOwnPropertyDescriptor(o, 'v').value, Object.getOwnPropertyNames(o)]", "[\"function\", 2, [\"g\", \"v\"]]"),
            ("const s = Symbol('k'), o = {[s]: 1}; [o[s], Symbol('k') === s, Symbol.for('x') === Symbol.for('x'), typeof Symbol.iterator]", "[1, false, true, \"string\"]"),
            ("const w = new WeakMap(); const k = {}; w.set(k, 2); [w.get(k), w.has({}), new WeakSet([k]).has(k)]", "[2, false, true]"),
            ("class P { get kind() { return 'p' } } class Q extends P { get kind() { return 'q' + super.kind } } new Q().kind", "qp"),
        ];
        for (src, want) in cases {
            assert_eq!(eval(src), want, "{src}");
        }
    }

    #[test]
    fn timers_and_logs() {
        let mut host = Bare::default();
        let mut rt = Runtime::new();
        rt.run(
            &mut host,
            "console.log('start', [1, 2], {k: 'v'}); var n = 0;\
             setTimeout(() => console.log('later', n), 50);\
             var id = setInterval(() => { n++; if (n == 3) clearInterval(id) }, 10);",
            "test",
        );
        assert_eq!(host.log, ["start [1, 2] {k: \"v\"}"]);
        assert_eq!(rt.next_timer(), Some(10.0));
        for t in [10.0, 20.0, 30.0, 40.0, 50.0] {
            rt.set_time(t, 0.0);
            rt.run_timers(&mut host);
        }
        assert_eq!(host.log.last().map(String::as_str), Some("later 3"));
        assert_eq!(rt.next_timer(), None, "the interval cleared itself");
    }

    #[test]
    fn stops_runaway_and_broken_scripts() {
        big_stack(stops_runaway_and_broken_scripts_inner);
    }

    fn stops_runaway_and_broken_scripts_inner() {
        let mut host = Bare::default();
        let mut rt = Runtime::new();
        rt.run(&mut host, "function* gen() {}", "a.js");
        assert!(host.log[0].starts_with("SyntaxError in a.js"));
        rt.run(&mut host, "undefinedFunction()", "b.js");
        assert!(host.log[1].contains("ReferenceError"), "{}", host.log[1]);
        rt.run(&mut host, "function r() { return r() } r()", "c.js");
        assert!(host.log[2].contains("RangeError"), "{}", host.log[2]);
        assert!(
            rt.stopped().is_none(),
            "errors do not stop the page's scripts"
        );
        rt.run(&mut host, "while (true) {}", "d.js");
        assert!(rt.stopped().is_some_and(|w| w.contains("too long")));
        rt.run(&mut host, "console.log('never')", "e.js");
        assert!(
            !host.log.iter().any(|l| l == "never"),
            "a stopped page runs nothing more"
        );
        let mut rt = Runtime::new();
        rt.run(
            &mut host,
            "var keep = []; while (true) keep.push({})",
            "f.js",
        );
        assert!(rt.stopped().is_some_and(|w| w.contains("objects")));
    }
}
