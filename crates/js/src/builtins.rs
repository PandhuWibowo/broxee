//! The global objects scripts expect (`window`, `document`, `console`, `Math`, `JSON`,
//! `Object`, `Array`, `String`, timers, `Map`, `Set`, `RegExp`) and the DOM bindings:
//! element properties and methods, `classList`, `style`, and event dispatch.

use std::sync::Arc;

use crate::interp::{
    Ctrl, Handler, Interp, Kind, Mode, ObjId, Pending, Resume, Target, Timer, Value,
};
use crate::regex::Regex;
use crate::Host;
use crate::{Reply, Request};

type R = Result<Value, Ctrl>;

fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(Value::Undefined)
}

fn num(it: &mut Interp, host: &mut dyn Host, args: &[Value], i: usize) -> Result<f64, Ctrl> {
    it.to_number(host, &arg(args, i))
}

fn string(it: &mut Interp, host: &mut dyn Host, v: &Value) -> Result<String, Ctrl> {
    Ok(it.to_string(host, v)?.to_string())
}

fn chars_of(it: &mut Interp, host: &mut dyn Host, this: &Value) -> Result<Vec<char>, Ctrl> {
    Ok(it.to_string(host, this)?.chars().collect())
}

/// A relative index (negative counts from the end), clamped to `0..=len`.
fn index(v: f64, len: usize) -> usize {
    let v = if v.is_nan() { 0.0 } else { v.trunc() };
    if v < 0.0 {
        (len as f64 + v).max(0.0) as usize
    } else {
        (v as usize).min(len)
    }
}

fn node_of(it: &Interp, v: &Value) -> Option<u32> {
    match v {
        Value::Obj(o) => match it.objects[*o as usize].kind {
            Kind::Element(n) => Some(n),
            _ => None,
        },
        _ => None,
    }
}

/// The one script object for DOM node `node`, so `a === b` holds for the same node.
pub fn element(it: &mut Interp, node: u32) -> R {
    if let Some(&o) = it.elements.get(&node) {
        return Ok(Value::Obj(o));
    }
    let p = it.protos.element;
    let o = it.alloc(Some(p), Kind::Element(node))?;
    it.elements.insert(node, o);
    Ok(Value::Obj(o))
}

fn elements(it: &mut Interp, nodes: Vec<u32>) -> R {
    let mut items = Vec::with_capacity(nodes.len());
    for n in nodes {
        items.push(element(it, n)?);
    }
    it.new_array(items)
}

fn array_items(it: &Interp, v: &Value) -> Option<Vec<Value>> {
    match v {
        Value::Obj(o) => match &it.objects[*o as usize].kind {
            Kind::Array(items) => Some(items.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// Run `f` on the items of array `this`.
fn with_array<T>(
    it: &mut Interp,
    this: &Value,
    f: impl FnOnce(&mut Vec<Value>) -> T,
) -> Result<T, Ctrl> {
    if let Value::Obj(o) = this {
        if let Kind::Array(items) = &mut it.objects[*o as usize].kind {
            return Ok(f(items));
        }
    }
    it.type_error("not an array")
}

// ---------------------------------------------------------------------------------------
// Installation.

pub fn install(it: &mut Interp) {
    let g = it.global;
    let gv = Value::Obj(g);
    for name in ["window", "self", "globalThis", "top", "parent", "frames"] {
        it.set_own(g, name, gv.clone());
    }
    install_object(it);
    install_function(it);
    install_array(it);
    install_string(it);
    install_number(it);
    install_errors(it);
    install_math(it);
    install_json(it);
    install_collections(it);
    install_symbol(it);
    install_promise(it);
    install_fetch(it);
    install_window(it);
    install_dom(it);
}

/// Header pairs as scripts give them: a `Headers`, an array of pairs, or a plain object.
fn header_pairs(
    it: &mut Interp,
    host: &mut dyn Host,
    v: &Value,
) -> Result<Vec<(String, String)>, Ctrl> {
    let mut out = Vec::new();
    if v.is_nullish() {
        return Ok(out);
    }
    if let Value::Obj(o) = v {
        if let Some(list) = it.get_own(*o, "__pairs") {
            for pair in it.iterate(host, &list)? {
                let kv = it.iterate(host, &pair)?;
                out.push((
                    string(it, host, &arg(&kv, 0))?,
                    string(it, host, &arg(&kv, 1))?,
                ));
            }
            return Ok(out);
        }
        if matches!(it.objects[*o as usize].kind, Kind::Array(_)) {
            for pair in it.iterate(host, v)? {
                let kv = it.iterate(host, &pair)?;
                out.push((
                    string(it, host, &arg(&kv, 0))?,
                    string(it, host, &arg(&kv, 1))?,
                ));
            }
            return Ok(out);
        }
    }
    for k in it.keys(v) {
        let value = it.get_value(host, v, &k)?;
        out.push((k.to_string(), string(it, host, &value)?));
    }
    Ok(out)
}

/// A request's headers, names lowercase; headers a script may not set are dropped (the
/// browser owns them), as is anything holding CR or LF.
fn clean_headers(pairs: Vec<(String, String)>) -> Vec<(String, String)> {
    const FORBIDDEN: &[&str] = &[
        "accept-charset",
        "accept-encoding",
        "access-control-request-headers",
        "access-control-request-method",
        "connection",
        "content-length",
        "cookie",
        "cookie2",
        "date",
        "dnt",
        "expect",
        "host",
        "keep-alive",
        "origin",
        "referer",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "user-agent",
        "via",
    ];
    pairs
        .into_iter()
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .filter(|(k, v)| {
            !k.is_empty()
                && k.bytes().all(|b| b.is_ascii_graphic() && b != b':')
                && !v.contains(['\r', '\n'])
                && !FORBIDDEN.contains(&k.as_str())
                && !k.starts_with("proxy-")
                && !k.starts_with("sec-")
        })
        .collect()
}

fn headers_object(it: &mut Interp, pairs: &[(String, String)]) -> Result<Value, Ctrl> {
    let proto = constructor_proto(it, "Headers");
    let o = it.alloc(Some(proto), Kind::Plain)?;
    let mut list = Vec::new();
    for (k, v) in pairs {
        let pair = it.new_array(vec![Value::str(&k.to_ascii_lowercase()), Value::str(v)])?;
        list.push(pair);
    }
    let list = it.new_array(list)?;
    it.set_own(o, "__pairs", list);
    Ok(Value::Obj(o))
}

/// The `[name, value]` arrays of a `Headers`.
fn header_list(it: &mut Interp, this: &Value) -> Result<ObjId, Ctrl> {
    match this {
        Value::Obj(o) => match it.get_own(*o, "__pairs") {
            Some(Value::Obj(list)) => Ok(list),
            _ => it.type_error("not a Headers object"),
        },
        _ => it.type_error("not a Headers object"),
    }
}

/// Send `request` for a script; `None` when the host refused it.
fn start_request(it: &mut Interp, host: &mut dyn Host, request: Request, pending: Pending) -> bool {
    match host.fetch(request) {
        Some(id) => {
            it.requests.insert(id, pending);
            true
        }
        None => false,
    }
}

/// `fetch`, `Headers`, `Response` and `XMLHttpRequest`. The host sends requests and checks
/// them (same origin, CORS, cookies); answers arrive later as tasks (`deliver`).
fn install_fetch(it: &mut Interp) {
    let g = it.global;
    it.method(g, "fetch", |it, host, _, args| {
        let input = arg(args, 0);
        let init = arg(args, 1);
        let url = match &input {
            Value::Obj(_) => match it.get_value(host, &input, "url")? {
                Value::Undefined => string(it, host, &input)?,
                u => string(it, host, &u)?,
            },
            v => string(it, host, v)?,
        };
        let get = |it: &mut Interp, host: &mut dyn Host, k: &str| -> Result<Value, Ctrl> {
            if init.is_nullish() {
                Ok(Value::Undefined)
            } else {
                it.get_value(host, &init, k)
            }
        };
        let method = match get(it, host, "method")? {
            Value::Undefined => "GET".to_string(),
            m => string(it, host, &m)?.to_ascii_uppercase(),
        };
        let headers = get(it, host, "headers")?;
        let mut headers = clean_headers(header_pairs(it, host, &headers)?);
        let body = match get(it, host, "body")? {
            v if v.is_nullish() => None,
            v => Some(string(it, host, &v)?),
        };
        if body.is_some() && !headers.iter().any(|(k, _)| k == "content-type") {
            headers.push(("content-type".into(), "text/plain;charset=UTF-8".into()));
        }
        let promise = it.new_promise()?;
        let request = Request {
            url,
            method,
            headers,
            body,
        };
        if !start_request(it, host, request, Pending::Fetch(promise)) {
            let e = it.make_error("TypeError", "Failed to fetch")?;
            it.settle(promise, false, e);
        }
        Ok(Value::Obj(promise))
    });

    // Headers
    let hp = it.alloc(Some(it.protos.object), Kind::Plain).unwrap_or(0);
    constructor(
        it,
        "Headers",
        |it, host, this, args| {
            let pairs = header_pairs(it, host, &arg(args, 0))?;
            let made = headers_object(it, &pairs)?;
            if let (Value::Obj(t), Value::Obj(m)) = (&this, &made) {
                if let Some(list) = it.get_own(*m, "__pairs") {
                    it.set_own(*t, "__pairs", list);
                    return Ok(this);
                }
            }
            Ok(made)
        },
        hp,
    );
    it.method(hp, "get", |it, host, this, args| {
        let list = header_list(it, &this)?;
        let name = string(it, host, &arg(args, 0))?.to_ascii_lowercase();
        let mut found: Vec<String> = Vec::new();
        for pair in it.iterate(host, &Value::Obj(list))? {
            let kv = it.iterate(host, &pair)?;
            if string(it, host, &arg(&kv, 0))? == name {
                found.push(string(it, host, &arg(&kv, 1))?);
            }
        }
        Ok(if found.is_empty() {
            Value::Null
        } else {
            Value::str(&found.join(", "))
        })
    });
    it.method(hp, "has", |it, host, this, args| {
        let get = it.get_value(host, &this, "get")?;
        let v = it.call(host, &get, this, args)?;
        Ok(Value::Bool(!matches!(v, Value::Null)))
    });
    for (name, replace) in [("set", true), ("append", false)] {
        let f: crate::interp::NativeFn = if replace {
            |it, host, this, args| header_put(it, host, &this, args, true)
        } else {
            |it, host, this, args| header_put(it, host, &this, args, false)
        };
        it.method(hp, name, f);
    }
    it.method(hp, "delete", |it, host, this, args| {
        let list = header_list(it, &this)?;
        let name = string(it, host, &arg(args, 0))?.to_ascii_lowercase();
        let pairs = it.iterate(host, &Value::Obj(list))?;
        let mut keep = Vec::new();
        for pair in pairs {
            let kv = it.iterate(host, &pair)?;
            if string(it, host, &arg(&kv, 0))? != name {
                keep.push(pair);
            }
        }
        if let Kind::Array(items) = &mut it.objects[list as usize].kind {
            *items = keep;
        }
        Ok(Value::Undefined)
    });
    it.method(hp, "forEach", |it, host, this, args| {
        let f = callback(it, args)?;
        let list = header_list(it, &this)?;
        for pair in it.iterate(host, &Value::Obj(list))? {
            let kv = it.iterate(host, &pair)?;
            it.call(
                host,
                &f,
                Value::Undefined,
                &[arg(&kv, 1), arg(&kv, 0), this.clone()],
            )?;
        }
        Ok(Value::Undefined)
    });
    it.method(hp, "entries", |it, host, this, _| {
        let list = header_list(it, &this)?;
        let all = it.iterate(host, &Value::Obj(list))?;
        it.new_array(all)
    });
    if let Some(f) = it.get_own(hp, "entries") {
        it.set_own(hp, "@@iterator", f);
    }

    // Response
    let rp = it.alloc(Some(it.protos.object), Kind::Plain).unwrap_or(0);
    constructor(
        it,
        "Response",
        |it, host, this, args| {
            let body = match arg(args, 0) {
                v if v.is_nullish() => String::new(),
                v => string(it, host, &v)?,
            };
            let reply = Reply {
                status: 200,
                body,
                ..Reply::default()
            };
            let made = response(it, &reply)?;
            let _ = this;
            Ok(made)
        },
        rp,
    );
    it.method(rp, "text", |it, host, this, _| {
        let body = response_body(it, &this)?;
        Ok(Value::Obj(it.promise_resolved(host, body)?))
    });
    it.method(rp, "json", |it, host, this, _| {
        let body = response_body(it, &this)?;
        let result = it.new_promise()?;
        let json = it.get_value(host, &Value::Obj(it.global), "JSON")?;
        let parse = it.get_value(host, &json, "parse")?;
        match it.call(host, &parse, json, &[body]) {
            Ok(v) => it.resolve_promise(host, result, v)?,
            Err(Ctrl::Throw(e)) => it.settle(result, false, e),
            Err(abort) => return Err(abort),
        }
        Ok(Value::Obj(result))
    });
    it.method(rp, "clone", |it, _, this, _| {
        let Value::Obj(o) = this else {
            return it.type_error("not a Response");
        };
        let props = it.objects[o as usize].props.clone();
        let proto = it.objects[o as usize].proto;
        let c = it.alloc(proto, Kind::Plain)?;
        it.objects[c as usize].props = props;
        Ok(Value::Obj(c))
    });
    for name in ["arrayBuffer", "blob", "formData"] {
        it.method(rp, name, |it, _, _, _| {
            let p = it.new_promise()?;
            let e = it.make_error("TypeError", "only text and JSON bodies are supported")?;
            it.settle(p, false, e);
            Ok(Value::Obj(p))
        });
    }

    // XMLHttpRequest
    let xp = it.alloc(Some(it.protos.object), Kind::Plain).unwrap_or(0);
    let xc = constructor(
        it,
        "XMLHttpRequest",
        |it, _, this, _| {
            let Value::Obj(o) = this else {
                return it.type_error("XMLHttpRequest needs new");
            };
            for (k, v) in [
                ("readyState", Value::Num(0.0)),
                ("status", Value::Num(0.0)),
                ("statusText", Value::str("")),
                ("responseText", Value::str("")),
                ("response", Value::str("")),
                ("responseType", Value::str("")),
                ("responseURL", Value::str("")),
                ("withCredentials", Value::Bool(false)),
                ("timeout", Value::Num(0.0)),
            ] {
                it.set_own(o, k, v);
            }
            Ok(this)
        },
        xp,
    );
    for (i, name) in ["UNSENT", "OPENED", "HEADERS_RECEIVED", "LOADING", "DONE"]
        .into_iter()
        .enumerate()
    {
        it.set_own(xp, name, Value::Num(i as f64));
        it.set_own(xc, name, Value::Num(i as f64));
    }
    it.method(xp, "open", |it, host, this, args| {
        let method = string(it, host, &arg(args, 0))?.to_ascii_uppercase();
        let url = string(it, host, &arg(args, 1))?;
        it.put(host, &this, "__method", Value::str(&method))?;
        it.put(host, &this, "__url", Value::str(&url))?;
        let empty = it.new_array(Vec::new())?;
        it.put(host, &this, "__headers", empty)?;
        it.put(host, &this, "readyState", Value::Num(1.0))?;
        Ok(Value::Undefined)
    });
    it.method(xp, "setRequestHeader", |it, host, this, args| {
        let list = it.get_value(host, &this, "__headers")?;
        let Value::Obj(list) = list else {
            return Err(it.throw_error("InvalidStateError", "open() first"));
        };
        let pair = it.new_array(vec![arg(args, 0), arg(args, 1)])?;
        if let Kind::Array(items) = &mut it.objects[list as usize].kind {
            items.push(pair);
        }
        Ok(Value::Undefined)
    });
    it.method(xp, "send", |it, host, this, args| {
        let Value::Obj(x) = this else {
            return it.type_error("not an XMLHttpRequest");
        };
        let method = it.get_value(host, &this, "__method")?;
        let method = string(it, host, &method)?;
        let url = it.get_value(host, &this, "__url")?;
        let url = string(it, host, &url)?;
        let headers = it.get_value(host, &this, "__headers")?;
        let mut headers = clean_headers(header_pairs(it, host, &headers)?);
        let body = match arg(args, 0) {
            v if v.is_nullish() || matches!(method.as_str(), "GET" | "HEAD") => None,
            v => Some(string(it, host, &v)?),
        };
        if body.is_some() && !headers.iter().any(|(k, _)| k == "content-type") {
            headers.push(("content-type".into(), "text/plain;charset=UTF-8".into()));
        }
        let request = Request {
            url,
            method,
            headers,
            body,
        };
        if !start_request(it, host, request, Pending::Xhr(x)) {
            xhr_done(it, host, x, Err("refused".into()))?;
        }
        Ok(Value::Undefined)
    });
    it.method(xp, "abort", |it, host, this, _| {
        if let Value::Obj(x) = this {
            it.requests
                .retain(|_, p| !matches!(p, Pending::Xhr(o) if *o == x));
        }
        it.put(host, &this, "readyState", Value::Num(0.0))?;
        Ok(Value::Undefined)
    });
    it.method(xp, "getResponseHeader", |it, host, this, args| {
        let h = it.get_value(host, &this, "__response_headers")?;
        if h.is_nullish() {
            return Ok(Value::Null);
        }
        let get = it.get_value(host, &h, "get")?;
        it.call(host, &get, h, args)
    });
    it.method(xp, "getAllResponseHeaders", |it, host, this, _| {
        let h = it.get_value(host, &this, "__response_headers")?;
        if h.is_nullish() {
            return Ok(Value::str(""));
        }
        let pairs = header_pairs(it, host, &h)?;
        let text: String = pairs.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
        Ok(Value::str(&text))
    });
    it.method(xp, "addEventListener", |it, host, this, args| {
        let kind = string(it, host, &arg(args, 0))?;
        let key = format!("__on_{kind}");
        let list = match it.get_value(host, &this, &key)? {
            Value::Obj(l) => l,
            _ => {
                let l = it.new_array(Vec::new())?;
                it.put(host, &this, &key, l.clone())?;
                let Value::Obj(l) = l else { unreachable!() };
                l
            }
        };
        if let Kind::Array(items) = &mut it.objects[list as usize].kind {
            items.push(arg(args, 1));
        }
        Ok(Value::Undefined)
    });
    it.method(xp, "removeEventListener", |it, host, this, args| {
        let kind = string(it, host, &arg(args, 0))?;
        if let Value::Obj(l) = it.get_value(host, &this, &format!("__on_{kind}"))? {
            let f = arg(args, 1);
            let same = |it: &Interp, v: &Value| it.strict_equals(v, &f);
            let keep: Vec<Value> = match &it.objects[l as usize].kind {
                Kind::Array(items) => items.iter().filter(|v| !same(it, v)).cloned().collect(),
                _ => Vec::new(),
            };
            if let Kind::Array(items) = &mut it.objects[l as usize].kind {
                *items = keep;
            }
        }
        Ok(Value::Undefined)
    });
}

fn header_put(
    it: &mut Interp,
    host: &mut dyn Host,
    this: &Value,
    args: &[Value],
    replace: bool,
) -> Result<Value, Ctrl> {
    let list = header_list(it, this)?;
    let name = string(it, host, &arg(args, 0))?.to_ascii_lowercase();
    let value = string(it, host, &arg(args, 1))?;
    if replace {
        let pairs = it.iterate(host, &Value::Obj(list))?;
        let mut keep = Vec::new();
        for pair in pairs {
            let kv = it.iterate(host, &pair)?;
            if string(it, host, &arg(&kv, 0))? != name {
                keep.push(pair);
            }
        }
        if let Kind::Array(items) = &mut it.objects[list as usize].kind {
            *items = keep;
        }
    }
    let pair = it.new_array(vec![Value::str(&name), Value::str(&value)])?;
    if let Kind::Array(items) = &mut it.objects[list as usize].kind {
        items.push(pair);
    }
    Ok(Value::Undefined)
}

fn response(it: &mut Interp, reply: &Reply) -> Result<Value, Ctrl> {
    let proto = constructor_proto(it, "Response");
    let o = it.alloc(Some(proto), Kind::Plain)?;
    let headers = headers_object(it, &reply.headers)?;
    for (k, v) in [
        ("status", Value::Num(f64::from(reply.status))),
        ("ok", Value::Bool((200..300).contains(&reply.status))),
        ("statusText", Value::str(reason(reply.status))),
        ("url", Value::str(&reply.url)),
        ("redirected", Value::Bool(false)),
        ("type", Value::str("basic")),
        ("headers", headers),
        ("__body", Value::str(&reply.body)),
    ] {
        it.set_own(o, k, v);
    }
    Ok(Value::Obj(o))
}

fn response_body(it: &mut Interp, this: &Value) -> Result<Value, Ctrl> {
    match this {
        Value::Obj(o) => Ok(it.get_own(*o, "__body").unwrap_or(Value::str(""))),
        _ => it.type_error("not a Response"),
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "",
    }
}

/// An `XMLHttpRequest` finished (or failed): set what it read and fire its events.
fn xhr_done(
    it: &mut Interp,
    host: &mut dyn Host,
    x: ObjId,
    result: Result<Reply, String>,
) -> Result<(), Ctrl> {
    let xv = Value::Obj(x);
    let events: &[&str] = match &result {
        Ok(reply) => {
            let kind = it.get_value(host, &xv, "responseType")?;
            let kind = string(it, host, &kind)?;
            let response = if kind == "json" {
                let json = it.get_value(host, &Value::Obj(it.global), "JSON")?;
                let parse = it.get_value(host, &json, "parse")?;
                it.call(host, &parse, json, &[Value::str(&reply.body)])
                    .unwrap_or(Value::Null)
            } else {
                Value::str(&reply.body)
            };
            let headers = headers_object(it, &reply.headers)?;
            for (k, v) in [
                ("status", Value::Num(f64::from(reply.status))),
                ("statusText", Value::str(reason(reply.status))),
                ("responseText", Value::str(&reply.body)),
                ("response", response),
                ("responseURL", Value::str(&reply.url)),
                ("__response_headers", headers),
            ] {
                it.set_own(x, k, v);
            }
            &["readystatechange", "load", "loadend"]
        }
        Err(_) => {
            it.set_own(x, "status", Value::Num(0.0));
            &["readystatechange", "error", "loadend"]
        }
    };
    it.set_own(x, "readyState", Value::Num(4.0));
    for kind in events {
        let event = it.new_object()?;
        it.put(host, &event, "type", Value::str(kind))?;
        it.put(host, &event, "target", xv.clone())?;
        let handler = it.get_value(host, &xv, &format!("on{kind}"))?;
        if it.is_callable(&handler) {
            it.call(host, &handler, xv.clone(), std::slice::from_ref(&event))?;
        }
        if let Value::Obj(l) = it.get_value(host, &xv, &format!("__on_{kind}"))? {
            let listeners = match &it.objects[l as usize].kind {
                Kind::Array(items) => items.clone(),
                _ => Vec::new(),
            };
            for f in listeners {
                if it.is_callable(&f) {
                    it.call(host, &f, xv.clone(), std::slice::from_ref(&event))?;
                }
            }
        }
    }
    Ok(())
}

/// The answer to request `id`.
pub fn deliver(
    it: &mut Interp,
    host: &mut dyn Host,
    id: u32,
    result: Result<Reply, String>,
) -> Result<(), Ctrl> {
    match it.requests.remove(&id) {
        Some(Pending::Fetch(promise)) => match result {
            Ok(reply) => {
                let r = response(it, &reply)?;
                it.resolve_promise(host, promise, r)
            }
            Err(_) => {
                let e = it.make_error("TypeError", "Failed to fetch")?;
                it.settle(promise, false, e);
                Ok(())
            }
        },
        Some(Pending::Xhr(x)) => xhr_done(it, host, x, result),
        None => Ok(()),
    }
}

/// `Promise` and `queueMicrotask`. Reactions run at the microtask checkpoint after each task.
fn install_promise(it: &mut Interp) {
    let p = it.protos.promise;
    let c = constructor(
        it,
        "Promise",
        |it, host, _, args| {
            let executor = arg(args, 0);
            if !it.is_callable(&executor) {
                return it.type_error("Promise needs an executor function");
            }
            let promise = it.new_promise()?;
            let (ok, err) = it.resolvers(promise)?;
            match it.call(host, &executor, Value::Undefined, &[ok, err]) {
                Err(Ctrl::Throw(e)) => it.settle(promise, false, e),
                Err(abort @ Ctrl::Abort(_)) => return Err(abort),
                _ => {}
            }
            Ok(Value::Obj(promise))
        },
        p,
    );
    fn this_promise(it: &mut Interp, this: &Value) -> Result<ObjId, Ctrl> {
        match it.is_promise(this) {
            Some(q) => Ok(q),
            None => it.type_error("not a promise"),
        }
    }
    it.method(p, "then", |it, _, this, args| {
        let q = this_promise(it, &this)?;
        Ok(Value::Obj(it.promise_then(
            q,
            arg(args, 0),
            arg(args, 1),
        )?))
    });
    it.method(p, "catch", |it, _, this, args| {
        let q = this_promise(it, &this)?;
        Ok(Value::Obj(it.promise_then(
            q,
            Value::Undefined,
            arg(args, 0),
        )?))
    });
    it.method(p, "finally", |it, _, this, args| {
        let q = this_promise(it, &this)?;
        let f = arg(args, 0);
        let h = if it.is_callable(&f) {
            Handler::Finally(f)
        } else {
            Handler::Pass
        };
        let result = it.new_promise()?;
        it.subscribe(q, h.clone(), h, Some(result));
        Ok(Value::Obj(result))
    });
    it.method(c, "resolve", |it, host, _, args| {
        Ok(Value::Obj(it.promise_resolved(host, arg(args, 0))?))
    });
    it.method(c, "reject", |it, _, _, args| {
        let r = it.new_promise()?;
        it.settle(r, false, arg(args, 0));
        Ok(Value::Obj(r))
    });
    it.method(c, "all", |it, host, _, args| {
        let items = it.iterate(host, &arg(args, 0))?;
        Ok(Value::Obj(it.promise_aggregate(host, items, Mode::All)?))
    });
    it.method(c, "allSettled", |it, host, _, args| {
        let items = it.iterate(host, &arg(args, 0))?;
        Ok(Value::Obj(it.promise_aggregate(
            host,
            items,
            Mode::AllSettled,
        )?))
    });
    it.method(c, "any", |it, host, _, args| {
        let items = it.iterate(host, &arg(args, 0))?;
        Ok(Value::Obj(it.promise_aggregate(host, items, Mode::Any)?))
    });
    it.method(c, "race", |it, host, _, args| {
        let items = it.iterate(host, &arg(args, 0))?;
        let result = it.new_promise()?;
        for item in items {
            let q = it.promise_resolved(host, item)?;
            it.subscribe(q, Handler::Pass, Handler::Pass, Some(result));
        }
        Ok(Value::Obj(result))
    });
    let gen = it.protos.generator;
    it.method(gen, "next", |it, host, this, args| {
        it.generator_resume(host, &this, Resume::Next(arg(args, 0)))
    });
    it.method(gen, "return", |it, host, this, args| {
        it.generator_resume(host, &this, Resume::Return(arg(args, 0)))
    });
    it.method(gen, "throw", |it, host, this, args| {
        it.generator_resume(host, &this, Resume::Throw(arg(args, 0)))
    });
    it.method(gen, "@@iterator", |_, _, this, _| Ok(this));
    let g = it.global;
    it.method(g, "queueMicrotask", |it, _, _, args| {
        let f = arg(args, 0);
        if !it.is_callable(&f) {
            return it.type_error("queueMicrotask needs a function");
        }
        it.queue_microtask(f);
        Ok(Value::Undefined)
    });
}

/// `Symbol`, as unique strings: they work as property keys that never collide, and the
/// well-known ones (`Symbol.iterator`) name the protocol hooks. `typeof` says "string".
/// `WeakMap` and `WeakSet` are `Map` and `Set`: a page's heap is dropped whole, so holding
/// keys weakly would change nothing.
fn install_symbol(it: &mut Interp) {
    let g = it.global;
    let c = it.native(|it, host, this, args| {
        let desc = match arg(args, 0) {
            Value::Undefined => Arc::from(""),
            v => it.to_string(host, &v)?,
        };
        let _ = this;
        let Some(Value::Obj(sym)) = it.get_own(it.global, "Symbol") else {
            return it.type_error("Symbol is gone");
        };
        let n = match it.get_own(sym, "__count") {
            Some(Value::Num(n)) => n + 1.0,
            _ => 1.0,
        };
        it.set_own(sym, "__count", Value::Num(n));
        Ok(Value::str(&format!("@@{desc}#{n}")))
    });
    for name in [
        "iterator",
        "asyncIterator",
        "hasInstance",
        "toPrimitive",
        "toStringTag",
        "species",
    ] {
        it.set_own(c, name, Value::str(&format!("@@{name}")));
    }
    it.method(c, "for", |it, host, _, args| {
        let key = it.to_string(host, &arg(args, 0))?;
        Ok(Value::str(&format!("@@for:{key}")))
    });
    it.set_own(g, "Symbol", Value::Obj(c));
    for (weak, strong) in [("WeakMap", "Map"), ("WeakSet", "Set")] {
        if let Some(v) = it.get_own(g, strong) {
            it.set_own(g, weak, v);
        }
    }
}

fn constructor(it: &mut Interp, name: &str, f: crate::interp::NativeFn, proto: ObjId) -> ObjId {
    let c = it.native(f);
    it.set_own(c, "prototype", Value::Obj(proto));
    it.set_own(proto, "constructor", Value::Obj(c));
    let g = it.global;
    it.set_own(g, name, Value::Obj(c));
    c
}

fn install_object(it: &mut Interp) {
    let p = it.protos.object;
    let c = constructor(
        it,
        "Object",
        |it, _, _, args| match arg(args, 0) {
            v @ Value::Obj(_) => Ok(v),
            _ => it.new_object(),
        },
        p,
    );
    it.method(p, "hasOwnProperty", |it, host, this, args| {
        let k = it.property_key(host, &arg(args, 0))?;
        Ok(Value::Bool(it.keys(&this).contains(&k)))
    });
    it.method(p, "toString", |it, _, this, _| {
        Ok(Value::str(match &this {
            Value::Obj(o) if matches!(it.objects[*o as usize].kind, Kind::Array(_)) => "",
            _ => "[object Object]",
        }))
    });
    it.method(p, "valueOf", |_, _, this, _| Ok(this));
    it.method(c, "keys", |it, _, _, args| {
        let keys = it.keys(&arg(args, 0)).into_iter().map(Value::Str).collect();
        it.new_array(keys)
    });
    // Every property is enumerable here, so the names are the keys.
    it.method(c, "getOwnPropertyNames", |it, _, _, args| {
        let keys = it.keys(&arg(args, 0)).into_iter().map(Value::Str).collect();
        it.new_array(keys)
    });
    it.method(c, "getOwnPropertyDescriptor", |it, host, _, args| {
        let o = arg(args, 0);
        let k = it.property_key(host, &arg(args, 1))?;
        let Value::Obj(on) = o else {
            return Ok(Value::Undefined);
        };
        let Some(v) = it.get_own(on, &k) else {
            return Ok(Value::Undefined);
        };
        let desc = it.new_object()?;
        let accessor = match &v {
            Value::Obj(a) => match &it.objects[*a as usize].kind {
                Kind::Accessor(g, s) => Some((g.clone(), s.clone())),
                _ => None,
            },
            _ => None,
        };
        match accessor {
            Some((g, s)) => {
                it.put(host, &desc, "get", g.unwrap_or(Value::Undefined))?;
                it.put(host, &desc, "set", s.unwrap_or(Value::Undefined))?;
            }
            None => {
                it.put(host, &desc, "value", v)?;
                it.put(host, &desc, "writable", Value::Bool(true))?;
            }
        }
        it.put(host, &desc, "enumerable", Value::Bool(true))?;
        it.put(host, &desc, "configurable", Value::Bool(true))?;
        Ok(desc)
    });
    it.method(c, "values", |it, host, _, args| {
        let o = arg(args, 0);
        let mut out = Vec::new();
        for k in it.keys(&o) {
            out.push(it.get_value(host, &o, &k)?);
        }
        it.new_array(out)
    });
    it.method(c, "entries", |it, host, _, args| {
        let o = arg(args, 0);
        let mut out = Vec::new();
        for k in it.keys(&o) {
            let v = it.get_value(host, &o, &k)?;
            out.push(it.new_array(vec![Value::Str(k), v])?);
        }
        it.new_array(out)
    });
    it.method(c, "assign", |it, host, _, args| {
        let target = arg(args, 0);
        for src in args.iter().skip(1) {
            for k in it.keys(src) {
                let v = it.get_value(host, src, &k)?;
                it.put(host, &target, &k, v)?;
            }
        }
        Ok(target)
    });
    it.method(c, "create", |it, _, _, args| {
        let proto = match arg(args, 0) {
            Value::Obj(p) => Some(p),
            _ => None,
        };
        Ok(Value::Obj(it.alloc(proto, Kind::Plain)?))
    });
    it.method(c, "getPrototypeOf", |it, _, _, args| {
        Ok(match arg(args, 0) {
            Value::Obj(o) => it.objects[o as usize].proto.map_or(Value::Null, Value::Obj),
            _ => Value::Null,
        })
    });
    it.method(c, "defineProperty", |it, host, _, args| {
        let (o, desc) = (arg(args, 0), arg(args, 2));
        let k = it.property_key(host, &arg(args, 1))?;
        let getter = it.get(host, &desc, "get")?.filter(|f| it.is_callable(f));
        let setter = it.get(host, &desc, "set")?.filter(|f| it.is_callable(f));
        if getter.is_some() || setter.is_some() {
            if let Value::Obj(on) = o {
                it.define_accessor(on, &k, getter, setter)?;
            }
        } else if let Some(v) = it.get(host, &desc, "value")? {
            it.put(host, &o, &k, v)?;
        }
        Ok(o)
    });
    for name in ["freeze", "seal", "preventExtensions"] {
        it.method(c, name, |it, _, _, args| {
            let o = arg(args, 0);
            if let Value::Obj(id) = o {
                it.frozen.insert(id);
            }
            Ok(o)
        });
    }
    for name in ["isFrozen", "isSealed"] {
        it.method(c, name, |it, _, _, args| {
            Ok(Value::Bool(match arg(args, 0) {
                Value::Obj(id) => it.frozen.contains(&id),
                _ => true,
            }))
        });
    }
    it.method(c, "isExtensible", |it, _, _, args| {
        Ok(Value::Bool(match arg(args, 0) {
            Value::Obj(id) => !it.frozen.contains(&id),
            _ => false,
        }))
    });
}

fn install_function(it: &mut Interp) {
    let p = it.protos.function;
    let g = it.global;
    let c = it.native(|it, _, _, _| it.type_error("Function() is not supported"));
    it.set_own(c, "prototype", Value::Obj(p));
    it.set_own(g, "Function", Value::Obj(c));
    it.method(p, "call", |it, host, this, args| {
        let rest = args.get(1..).map(<[Value]>::to_vec).unwrap_or_default();
        it.call(host, &this, arg(args, 0), &rest)
    });
    it.method(p, "apply", |it, host, this, args| {
        let list = match arg(args, 1) {
            v if v.is_nullish() => Vec::new(),
            v => it.iterate(host, &v)?,
        };
        it.call(host, &this, arg(args, 0), &list)
    });
    it.method(p, "bind", |it, _, this, args| {
        let Value::Obj(target) = this else {
            return it.type_error("bind needs a function");
        };
        let rest = args.get(1..).map(<[Value]>::to_vec).unwrap_or_default();
        let fp = it.protos.function;
        Ok(Value::Obj(it.alloc(
            Some(fp),
            Kind::Bound(target, arg(args, 0), rest),
        )?))
    });
    it.method(p, "toString", |_, _, _, _| {
        Ok(Value::str("function () { [code] }"))
    });
}

fn callback(it: &mut Interp, args: &[Value]) -> Result<Value, Ctrl> {
    let f = arg(args, 0);
    if !it.is_callable(&f) {
        return it.type_error("callback is not a function");
    }
    Ok(f)
}

fn install_array(it: &mut Interp) {
    let p = it.protos.array;
    let c = constructor(
        it,
        "Array",
        |it, host, _, args| {
            if let [Value::Num(n)] = args {
                if *n < 0.0 || *n > 1e7 || n.fract() != 0.0 {
                    return Err(it.throw_error("RangeError", "invalid array length"));
                }
                return it.new_array(vec![Value::Undefined; *n as usize]);
            }
            let _ = host;
            it.new_array(args.to_vec())
        },
        p,
    );
    it.method(c, "isArray", |it, _, _, args| {
        Ok(Value::Bool(array_items(it, &arg(args, 0)).is_some()))
    });
    it.method(c, "of", |it, _, _, args| it.new_array(args.to_vec()));
    it.method(c, "from", |it, host, _, args| {
        let src = arg(args, 0);
        let mut items = if src.is_nullish() {
            Vec::new()
        } else {
            it.iterate(host, &src)?
        };
        let f = arg(args, 1);
        if it.is_callable(&f) {
            for (i, item) in items.iter_mut().enumerate() {
                *item = it.call(
                    host,
                    &f,
                    Value::Undefined,
                    &[item.clone(), Value::Num(i as f64)],
                )?;
            }
        }
        it.new_array(items)
    });
    it.method(p, "push", |it, _, this, args| {
        with_array(it, &this, |a| {
            a.extend_from_slice(args);
            Value::Num(a.len() as f64)
        })
    });
    it.method(p, "pop", |it, _, this, _| {
        with_array(it, &this, |a| a.pop().unwrap_or(Value::Undefined))
    });
    it.method(p, "shift", |it, _, this, _| {
        with_array(it, &this, |a| {
            if a.is_empty() {
                Value::Undefined
            } else {
                a.remove(0)
            }
        })
    });
    it.method(p, "unshift", |it, _, this, args| {
        with_array(it, &this, |a| {
            a.splice(0..0, args.iter().cloned());
            Value::Num(a.len() as f64)
        })
    });
    it.method(p, "slice", |it, host, this, args| {
        let items = it.iterate(host, &this)?;
        let len = items.len();
        let s = index(num(it, host, args, 0)?, len);
        let e = match arg(args, 1) {
            Value::Undefined => len,
            v => index(it.to_number(host, &v)?, len),
        };
        it.new_array(
            items
                .get(s..e.max(s))
                .map(<[Value]>::to_vec)
                .unwrap_or_default(),
        )
    });
    it.method(p, "splice", |it, host, this, args| {
        let len = with_array(it, &this, |a| a.len())?;
        let s = index(num(it, host, args, 0)?, len);
        let count = match arg(args, 1) {
            Value::Undefined if args.len() < 2 => len - s,
            v => (it.to_number(host, &v)?.max(0.0) as usize).min(len - s),
        };
        let insert = args.get(2..).map(<[Value]>::to_vec).unwrap_or_default();
        let removed: Vec<Value> =
            with_array(it, &this, |a| a.splice(s..s + count, insert).collect())?;
        it.new_array(removed)
    });
    it.method(p, "concat", |it, host, this, args| {
        let mut items = it.iterate(host, &this)?;
        for a in args {
            match array_items(it, a) {
                Some(more) => items.extend(more),
                None => items.push(a.clone()),
            }
        }
        it.new_array(items)
    });
    it.method(p, "join", |it, host, this, args| {
        let sep = match arg(args, 0) {
            Value::Undefined => ",".to_string(),
            v => string(it, host, &v)?,
        };
        let items = it.iterate(host, &this)?;
        let mut parts = Vec::with_capacity(items.len());
        for v in &items {
            parts.push(if v.is_nullish() {
                String::new()
            } else {
                string(it, host, v)?
            });
        }
        Ok(Value::str(&parts.join(&sep)))
    });
    it.method(p, "toString", |it, host, this, _| {
        let join = it.get_value(host, &this, "join")?;
        it.call(host, &join, this, &[])
    });
    it.method(p, "indexOf", |it, host, this, args| {
        let items = it.iterate(host, &this)?;
        let x = arg(args, 0);
        Ok(Value::Num(
            items
                .iter()
                .position(|v| it.strict_equals(v, &x))
                .map_or(-1.0, |i| i as f64),
        ))
    });
    it.method(p, "lastIndexOf", |it, host, this, args| {
        let items = it.iterate(host, &this)?;
        let x = arg(args, 0);
        Ok(Value::Num(
            items
                .iter()
                .rposition(|v| it.strict_equals(v, &x))
                .map_or(-1.0, |i| i as f64),
        ))
    });
    it.method(p, "includes", |it, host, this, args| {
        let items = it.iterate(host, &this)?;
        let x = arg(args, 0);
        Ok(Value::Bool(items.iter().any(|v| {
            it.strict_equals(v, &x)
                || matches!((v, &x), (Value::Num(a), Value::Num(b)) if a.is_nan() && b.is_nan())
        })))
    });
    it.method(p, "reverse", |it, _, this, _| {
        with_array(it, &this, |a| a.reverse())?;
        Ok(this)
    });
    it.method(p, "fill", |it, _, this, args| {
        let v = arg(args, 0);
        with_array(it, &this, |a| a.iter_mut().for_each(|x| *x = v.clone()))?;
        Ok(this)
    });
    it.method(p, "flat", |it, _, this, _| {
        let items = array_items(it, &this).unwrap_or_default();
        let mut out = Vec::new();
        for v in items {
            match array_items(it, &v) {
                Some(inner) => out.extend(inner),
                None => out.push(v),
            }
        }
        it.new_array(out)
    });
    it.method(p, "forEach", |it, host, this, args| {
        let f = callback(it, args)?;
        for (i, v) in it.iterate(host, &this)?.into_iter().enumerate() {
            it.call(
                host,
                &f,
                arg(args, 1),
                &[v, Value::Num(i as f64), this.clone()],
            )?;
        }
        Ok(Value::Undefined)
    });
    it.method(p, "map", |it, host, this, args| {
        let f = callback(it, args)?;
        let mut out = Vec::new();
        for (i, v) in it.iterate(host, &this)?.into_iter().enumerate() {
            out.push(it.call(
                host,
                &f,
                arg(args, 1),
                &[v, Value::Num(i as f64), this.clone()],
            )?);
        }
        it.new_array(out)
    });
    it.method(p, "filter", |it, host, this, args| {
        let f = callback(it, args)?;
        let mut out = Vec::new();
        for (i, v) in it.iterate(host, &this)?.into_iter().enumerate() {
            let keep = it.call(
                host,
                &f,
                arg(args, 1),
                &[v.clone(), Value::Num(i as f64), this.clone()],
            )?;
            if it.truthy(&keep) {
                out.push(v);
            }
        }
        it.new_array(out)
    });
    it.method(p, "flatMap", |it, host, this, args| {
        let f = callback(it, args)?;
        let mut out = Vec::new();
        for (i, v) in it.iterate(host, &this)?.into_iter().enumerate() {
            let r = it.call(
                host,
                &f,
                arg(args, 1),
                &[v, Value::Num(i as f64), this.clone()],
            )?;
            match array_items(it, &r) {
                Some(inner) => out.extend(inner),
                None => out.push(r),
            }
        }
        it.new_array(out)
    });
    fn search(
        it: &mut Interp,
        host: &mut dyn Host,
        this: &Value,
        args: &[Value],
    ) -> Result<Option<(usize, Value)>, Ctrl> {
        let f = callback(it, args)?;
        for (i, v) in it.iterate(host, this)?.into_iter().enumerate() {
            let r = it.call(
                host,
                &f,
                arg(args, 1),
                &[v.clone(), Value::Num(i as f64), this.clone()],
            )?;
            if it.truthy(&r) {
                return Ok(Some((i, v)));
            }
        }
        Ok(None)
    }
    it.method(p, "find", |it, host, this, args| {
        Ok(search(it, host, &this, args)?.map_or(Value::Undefined, |f| f.1))
    });
    it.method(p, "findIndex", |it, host, this, args| {
        Ok(Value::Num(
            search(it, host, &this, args)?.map_or(-1.0, |f| f.0 as f64),
        ))
    });
    it.method(p, "some", |it, host, this, args| {
        Ok(Value::Bool(search(it, host, &this, args)?.is_some()))
    });
    it.method(p, "every", |it, host, this, args| {
        let f = callback(it, args)?;
        for (i, v) in it.iterate(host, &this)?.into_iter().enumerate() {
            let r = it.call(
                host,
                &f,
                arg(args, 1),
                &[v, Value::Num(i as f64), this.clone()],
            )?;
            if !it.truthy(&r) {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });
    it.method(p, "reduce", |it, host, this, args| {
        let f = callback(it, args)?;
        let items = it.iterate(host, &this)?;
        let mut iter = items.into_iter().enumerate();
        let mut acc = if args.len() > 1 {
            arg(args, 1)
        } else {
            match iter.next() {
                Some((_, v)) => v,
                None => return it.type_error("reduce of an empty array with no initial value"),
            }
        };
        for (i, v) in iter {
            acc = it.call(
                host,
                &f,
                Value::Undefined,
                &[acc, v, Value::Num(i as f64), this.clone()],
            )?;
        }
        Ok(acc)
    });
    it.method(p, "sort", |it, host, this, args| {
        let mut items = array_items(it, &this).unwrap_or_default();
        let f = arg(args, 0);
        // Insertion sort through the comparator: stable, and errors propagate.
        for i in 1..items.len() {
            let mut j = i;
            while j > 0 {
                let before = if it.is_callable(&f) {
                    let r = it.call(
                        host,
                        &f,
                        Value::Undefined,
                        &[items[j].clone(), items[j - 1].clone()],
                    )?;
                    it.to_number(host, &r)? < 0.0
                } else {
                    let (a, b) = (items[j].clone(), items[j - 1].clone());
                    if a.is_nullish() {
                        false
                    } else {
                        let (x, y) = (string(it, host, &a)?, string(it, host, &b)?);
                        b.is_nullish() || x < y
                    }
                };
                if !before {
                    break;
                }
                items.swap(j, j - 1);
                j -= 1;
            }
        }
        with_array(it, &this, |a| *a = items)?;
        Ok(this)
    });
    it.method(p, "keys", |it, host, this, _| {
        let n = it.iterate(host, &this)?.len();
        it.new_array((0..n).map(|i| Value::Num(i as f64)).collect())
    });
    it.method(p, "values", |it, host, this, _| {
        let items = it.iterate(host, &this)?;
        it.new_array(items)
    });
    it.method(p, "entries", |it, host, this, _| {
        let items = it.iterate(host, &this)?;
        let mut out = Vec::new();
        for (i, v) in items.into_iter().enumerate() {
            out.push(it.new_array(vec![Value::Num(i as f64), v])?);
        }
        it.new_array(out)
    });
}

pub fn regex(it: &mut Interp, literal: &str) -> R {
    let end = literal.rfind('/').unwrap_or(0);
    let (pattern, flags) = (&literal[1..end.max(1)], &literal[end + 1..]);
    make_regex(it, pattern, flags)
}

fn make_regex(it: &mut Interp, pattern: &str, flags: &str) -> R {
    let re = match Regex::new(pattern, flags) {
        Ok(re) => re,
        Err(e) => {
            return Err(it.throw_error(
                "SyntaxError",
                &format!("invalid regular expression /{pattern}/: {e}"),
            ))
        }
    };
    let proto = match it.get_own(it.global, "RegExp").and_then(|c| match c {
        Value::Obj(c) => it.get_own(c, "prototype"),
        _ => None,
    }) {
        Some(Value::Obj(p)) => p,
        _ => it.protos.object,
    };
    let o = it.alloc(Some(proto), Kind::RegExp(Arc::new(re)))?;
    it.set_own(o, "source", Value::str(pattern));
    it.set_own(o, "flags", Value::str(flags));
    it.set_own(o, "global", Value::Bool(flags.contains('g')));
    it.set_own(o, "lastIndex", Value::Num(0.0));
    Ok(Value::Obj(o))
}

fn regex_of(it: &Interp, v: &Value) -> Option<Arc<Regex>> {
    match v {
        Value::Obj(o) => match &it.objects[*o as usize].kind {
            Kind::RegExp(re) => Some(re.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// A match as an array: the matched text, then the groups; with `index`.
fn match_array(it: &mut Interp, text: &[char], caps: &[Option<(usize, usize)>]) -> R {
    let items: Vec<Value> = caps
        .iter()
        .map(|c| {
            c.map_or(Value::Undefined, |(s, e)| {
                Value::str(&text[s..e].iter().collect::<String>())
            })
        })
        .collect();
    let a = it.new_array(items)?;
    if let (Value::Obj(o), Some(Some((s, _)))) = (&a, caps.first()) {
        it.set_own(*o, "index", Value::Num(*s as f64));
    }
    Ok(a)
}

/// The replacement for one match: `$&`, `$1`… in a string, or a function's result.
fn replacement(
    it: &mut Interp,
    host: &mut dyn Host,
    with: &Value,
    text: &[char],
    caps: &[Option<(usize, usize)>],
) -> Result<String, Ctrl> {
    let piece =
        |c: &Option<(usize, usize)>| c.map_or(String::new(), |(s, e)| text[s..e].iter().collect());
    if it.is_callable(with) {
        let mut args: Vec<Value> = caps.iter().map(|c| Value::str(&piece(c))).collect();
        args.push(Value::Num(caps[0].map_or(0, |c| c.0) as f64));
        args.push(Value::str(&text.iter().collect::<String>()));
        let r = it.call(host, with, Value::Undefined, &args)?;
        return string(it, host, &r);
    }
    let w = string(it, host, with)?;
    let mut out = String::new();
    let mut chars = w.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('$') => {
                chars.next();
                out.push('$');
            }
            Some('&') => {
                chars.next();
                out.push_str(&piece(&caps[0]));
            }
            Some(d) if d.is_ascii_digit() => {
                chars.next();
                let g = d.to_digit(10).unwrap_or(0) as usize;
                match caps.get(g) {
                    Some(c) if g > 0 => out.push_str(&piece(c)),
                    _ => {
                        out.push('$');
                        out.push(d);
                    }
                }
            }
            _ => out.push('$'),
        }
    }
    Ok(out)
}

fn install_string(it: &mut Interp) {
    let p = it.protos.string;
    let c = constructor(
        it,
        "String",
        |it, host, _, args| {
            if args.is_empty() {
                return Ok(Value::str(""));
            }
            Ok(Value::Str(it.to_string(host, &args[0])?))
        },
        p,
    );
    it.method(c, "fromCharCode", |it, host, _, args| {
        let mut s = String::new();
        for a in args {
            let n = it.to_number(host, a)? as u32;
            s.push(char::from_u32(n).unwrap_or('\u{fffd}'));
        }
        Ok(Value::str(&s))
    });
    let re_proto = it.alloc(Some(it.protos.object), Kind::Plain).unwrap_or(0);
    constructor(
        it,
        "RegExp",
        |it, host, _, args| {
            let src = match regex_of(it, &arg(args, 0)) {
                Some(_) => it.get_value(host, &arg(args, 0), "source")?,
                None => arg(args, 0),
            };
            let pattern = string(it, host, &src)?;
            let flags = match arg(args, 1) {
                Value::Undefined => String::new(),
                v => string(it, host, &v)?,
            };
            make_regex(it, &pattern, &flags)
        },
        re_proto,
    );
    it.method(re_proto, "test", |it, host, this, args| {
        let Some(re) = regex_of(it, &this) else {
            return it.type_error("not a regular expression");
        };
        let text = chars_of(it, host, &arg(args, 0))?;
        let start = if re.global {
            it.get_value(host, &this, "lastIndex")?.clone()
        } else {
            Value::Num(0.0)
        };
        let start = index(it.to_number(host, &start)?, text.len());
        let found = re.find_at(&text, start);
        if re.global {
            let next = found
                .as_ref()
                .and_then(|c| c[0])
                .map_or(0, |c| c.1.max(c.0 + 1));
            it.put(host, &this, "lastIndex", Value::Num(next as f64))?;
        }
        Ok(Value::Bool(found.is_some()))
    });
    it.method(re_proto, "exec", |it, host, this, args| {
        let Some(re) = regex_of(it, &this) else {
            return it.type_error("not a regular expression");
        };
        let text = chars_of(it, host, &arg(args, 0))?;
        let start = if re.global {
            it.get_value(host, &this, "lastIndex")?
        } else {
            Value::Num(0.0)
        };
        let start = index(it.to_number(host, &start)?, text.len());
        match re.find_at(&text, start) {
            Some(caps) => {
                if re.global {
                    let (s, e) = caps[0].unwrap_or((0, 0));
                    it.put(host, &this, "lastIndex", Value::Num(e.max(s + 1) as f64))?;
                }
                match_array(it, &text, &caps)
            }
            None => {
                it.put(host, &this, "lastIndex", Value::Num(0.0))?;
                Ok(Value::Null)
            }
        }
    });
    it.method(re_proto, "toString", |it, host, this, _| {
        let s = it.get_value(host, &this, "source")?;
        let f = it.get_value(host, &this, "flags")?;
        let (s, f) = (string(it, host, &s)?, string(it, host, &f)?);
        Ok(Value::str(&format!("/{s}/{f}")))
    });

    macro_rules! chars_method {
        ($name:literal, |$it:ident, $host:ident, $s:ident, $args:ident| $body:expr) => {
            it.method(p, $name, |$it, $host, this, $args| {
                let $s = chars_of($it, $host, &this)?;
                $body
            });
        };
    }
    chars_method!("charAt", |it, host, s, args| {
        let i = num(it, host, args, 0)?;
        Ok(Value::str(
            &s.get(i as usize).map_or(String::new(), |c| c.to_string()),
        ))
    });
    chars_method!("charCodeAt", |it, host, s, args| {
        let i = num(it, host, args, 0)?;
        Ok(Value::Num(
            s.get(if i.is_nan() { 0 } else { i as usize })
                .map_or(f64::NAN, |&c| f64::from(c as u32)),
        ))
    });
    chars_method!("codePointAt", |it, host, s, args| {
        let i = num(it, host, args, 0)?;
        Ok(s.get(if i.is_nan() { 0 } else { i as usize })
            .map_or(Value::Undefined, |&c| Value::Num(f64::from(c as u32))))
    });
    chars_method!("slice", |it, host, s, args| {
        let a = index(num(it, host, args, 0)?, s.len());
        let b = match arg(args, 1) {
            Value::Undefined => s.len(),
            v => index(it.to_number(host, &v)?, s.len()),
        };
        Ok(Value::str(&s[a..b.max(a)].iter().collect::<String>()))
    });
    chars_method!("substring", |it, host, s, args| {
        let clamp = |v: f64| {
            if v.is_nan() {
                0
            } else {
                v.max(0.0).min(s.len() as f64) as usize
            }
        };
        let a = clamp(num(it, host, args, 0)?);
        let b = match arg(args, 1) {
            Value::Undefined => s.len(),
            v => clamp(it.to_number(host, &v)?),
        };
        let (a, b) = (a.min(b), a.max(b));
        Ok(Value::str(&s[a..b].iter().collect::<String>()))
    });
    chars_method!("substr", |it, host, s, args| {
        let a = index(num(it, host, args, 0)?, s.len());
        let n = match arg(args, 1) {
            Value::Undefined => s.len(),
            v => it.to_number(host, &v)?.max(0.0) as usize,
        };
        Ok(Value::str(
            &s[a..(a + n).min(s.len())].iter().collect::<String>(),
        ))
    });
    chars_method!("indexOf", |it, host, s, args| {
        let needle = chars_of(it, host, &arg(args, 0))?;
        let from = index(num(it, host, args, 1)?, s.len());
        Ok(Value::Num(
            find_chars(&s, &needle, from).map_or(-1.0, |i| i as f64),
        ))
    });
    chars_method!("lastIndexOf", |it, host, s, args| {
        let needle = chars_of(it, host, &arg(args, 0))?;
        let found = (0..=s.len().saturating_sub(needle.len()))
            .rev()
            .find(|&i| s[i..].starts_with(&needle));
        Ok(Value::Num(found.map_or(-1.0, |i| i as f64)))
    });
    chars_method!("includes", |it, host, s, args| {
        let needle = chars_of(it, host, &arg(args, 0))?;
        Ok(Value::Bool(find_chars(&s, &needle, 0).is_some()))
    });
    chars_method!("startsWith", |it, host, s, args| {
        let needle = chars_of(it, host, &arg(args, 0))?;
        let from = index(num(it, host, args, 1)?, s.len());
        Ok(Value::Bool(s[from..].starts_with(&needle)))
    });
    chars_method!("endsWith", |it, host, s, args| {
        let needle = chars_of(it, host, &arg(args, 0))?;
        Ok(Value::Bool(s.ends_with(&needle)))
    });
    chars_method!("search", |it, host, s, args| {
        let re = match regex_of(it, &arg(args, 0)) {
            Some(re) => re,
            None => {
                let p = string(it, host, &arg(args, 0))?;
                let v = make_regex(it, &p, "")?;
                regex_of(it, &v).ok_or_else(|| it.throw_error("TypeError", "bad pattern"))?
            }
        };
        Ok(Value::Num(
            re.find_at(&s, 0)
                .and_then(|c| c[0])
                .map_or(-1.0, |c| c.0 as f64),
        ))
    });
    chars_method!("match", |it, host, s, args| {
        let re = match regex_of(it, &arg(args, 0)) {
            Some(re) => re,
            None => {
                let p = string(it, host, &arg(args, 0))?;
                let v = make_regex(it, &p, "")?;
                regex_of(it, &v).ok_or_else(|| it.throw_error("TypeError", "bad pattern"))?
            }
        };
        if !re.global {
            return match re.find_at(&s, 0) {
                Some(caps) => match_array(it, &s, &caps),
                None => Ok(Value::Null),
            };
        }
        let mut out = Vec::new();
        let mut at = 0;
        while let Some(caps) = re.find_at(&s, at) {
            let (a, b) = caps[0].unwrap_or((at, at));
            out.push(Value::str(&s[a..b].iter().collect::<String>()));
            at = b.max(a + 1);
            if at > s.len() {
                break;
            }
        }
        if out.is_empty() {
            Ok(Value::Null)
        } else {
            it.new_array(out)
        }
    });
    fn replace(it: &mut Interp, host: &mut dyn Host, this: Value, args: &[Value], all: bool) -> R {
        let s = chars_of(it, host, &this)?;
        let with = arg(args, 1);
        let mut out = String::new();
        if let Some(re) = regex_of(it, &arg(args, 0)) {
            let mut at = 0;
            let mut copied = 0;
            while let Some(caps) = re.find_at(&s, at) {
                let (a, b) = caps[0].unwrap_or((at, at));
                out.extend(&s[copied..a]);
                out.push_str(&replacement(it, host, &with, &s, &caps)?);
                copied = b;
                at = b.max(a + 1);
                if !(re.global || all) || at > s.len() {
                    break;
                }
            }
            out.extend(&s[copied.min(s.len())..]);
        } else {
            let needle = chars_of(it, host, &arg(args, 0))?;
            let mut at = 0;
            let mut copied = 0;
            while let Some(i) = find_chars(&s, &needle, at) {
                out.extend(&s[copied..i]);
                let caps = [Some((i, i + needle.len()))];
                out.push_str(&replacement(it, host, &with, &s, &caps)?);
                copied = i + needle.len();
                at = copied + usize::from(needle.is_empty());
                if !all || at > s.len() {
                    break;
                }
            }
            out.extend(&s[copied.min(s.len())..]);
        }
        Ok(Value::str(&out))
    }
    it.method(p, "replace", |it, host, this, args| {
        replace(it, host, this, args, false)
    });
    it.method(p, "replaceAll", |it, host, this, args| {
        replace(it, host, this, args, true)
    });
    chars_method!("split", |it, host, s, args| {
        let sep = arg(args, 0);
        let limit = match arg(args, 1) {
            Value::Undefined => usize::MAX,
            v => it.to_number(host, &v)? as usize,
        };
        let text: String = s.iter().collect();
        let mut parts: Vec<String> = if sep.is_nullish() {
            vec![text]
        } else if let Some(re) = regex_of(it, &sep) {
            let mut out = Vec::new();
            let (mut at, mut copied) = (0, 0);
            while at < s.len() {
                let Some(caps) = re.find_at(&s, at) else {
                    break;
                };
                let (a, b) = caps[0].unwrap_or((at, at));
                if b == a && a >= s.len() {
                    break;
                }
                if b > copied || a > copied {
                    out.push(s[copied..a].iter().collect());
                    copied = b;
                }
                at = b.max(a + 1);
            }
            out.push(s[copied.min(s.len())..].iter().collect());
            out
        } else {
            let sep = string(it, host, &sep)?;
            if sep.is_empty() {
                s.iter().map(|c| c.to_string()).collect()
            } else {
                text.split(sep.as_str()).map(str::to_string).collect()
            }
        };
        parts.truncate(limit);
        let items = parts.iter().map(|p| Value::str(p)).collect();
        it.new_array(items)
    });
    it.method(p, "toUpperCase", |it, host, this, _| {
        Ok(Value::str(&it.to_string(host, &this)?.to_uppercase()))
    });
    it.method(p, "toLowerCase", |it, host, this, _| {
        Ok(Value::str(&it.to_string(host, &this)?.to_lowercase()))
    });
    it.method(p, "toLocaleUpperCase", |it, host, this, _| {
        Ok(Value::str(&it.to_string(host, &this)?.to_uppercase()))
    });
    it.method(p, "toLocaleLowerCase", |it, host, this, _| {
        Ok(Value::str(&it.to_string(host, &this)?.to_lowercase()))
    });
    it.method(p, "trim", |it, host, this, _| {
        Ok(Value::str(it.to_string(host, &this)?.trim()))
    });
    it.method(p, "trimStart", |it, host, this, _| {
        Ok(Value::str(it.to_string(host, &this)?.trim_start()))
    });
    it.method(p, "trimEnd", |it, host, this, _| {
        Ok(Value::str(it.to_string(host, &this)?.trim_end()))
    });
    it.method(p, "toString", |it, host, this, _| {
        Ok(Value::Str(it.to_string(host, &this)?))
    });
    it.method(p, "valueOf", |it, host, this, _| {
        Ok(Value::Str(it.to_string(host, &this)?))
    });
    it.method(p, "concat", |it, host, this, args| {
        let mut s = string(it, host, &this)?;
        for a in args {
            s.push_str(&it.to_string(host, a)?);
        }
        Ok(Value::str(&s))
    });
    it.method(p, "repeat", |it, host, this, args| {
        let s = string(it, host, &this)?;
        let n = num(it, host, args, 0)?;
        if n < 0.0 || !n.is_finite() || s.len() as f64 * n > 1e7 {
            return Err(it.throw_error("RangeError", "invalid count"));
        }
        Ok(Value::str(&s.repeat(n as usize)))
    });
    fn pad(it: &mut Interp, host: &mut dyn Host, this: Value, args: &[Value], start: bool) -> R {
        let s = string(it, host, &this)?;
        let target = it.to_number(host, &arg(args, 0))?.min(1e6) as usize;
        let fill = match arg(args, 1) {
            Value::Undefined => " ".to_string(),
            v => string(it, host, &v)?,
        };
        let len = s.chars().count();
        if target <= len || fill.is_empty() {
            return Ok(Value::str(&s));
        }
        let padding: String = fill.chars().cycle().take(target - len).collect();
        Ok(Value::str(&if start { padding + &s } else { s + &padding }))
    }
    it.method(p, "padStart", |it, host, this, args| {
        pad(it, host, this, args, true)
    });
    it.method(p, "padEnd", |it, host, this, args| {
        pad(it, host, this, args, false)
    });
}

fn find_chars(s: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(s.len()));
    }
    (from..=s.len().saturating_sub(needle.len())).find(|&i| s[i..].starts_with(needle))
}

fn install_number(it: &mut Interp) {
    let p = it.protos.number;
    let c = constructor(
        it,
        "Number",
        |it, host, _, args| {
            if args.is_empty() {
                return Ok(Value::Num(0.0));
            }
            Ok(Value::Num(it.to_number(host, &args[0])?))
        },
        p,
    );
    let bp = it.protos.boolean;
    constructor(
        it,
        "Boolean",
        |it, _, _, args| Ok(Value::Bool(it.truthy(&arg(args, 0)))),
        bp,
    );
    it.method(bp, "toString", |it, _, this, _| {
        Ok(Value::str(if it.truthy(&this) { "true" } else { "false" }))
    });
    it.set_own(c, "MAX_SAFE_INTEGER", Value::Num(9007199254740991.0));
    it.set_own(c, "MIN_SAFE_INTEGER", Value::Num(-9007199254740991.0));
    it.set_own(c, "EPSILON", Value::Num(f64::EPSILON));
    it.set_own(c, "MAX_VALUE", Value::Num(f64::MAX));
    it.set_own(c, "POSITIVE_INFINITY", Value::Num(f64::INFINITY));
    it.set_own(c, "NEGATIVE_INFINITY", Value::Num(f64::NEG_INFINITY));
    it.set_own(c, "NaN", Value::Num(f64::NAN));
    it.method(c, "isInteger", |_, _, _, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Num(n) if n.is_finite() && n.fract() == 0.0),
        ))
    });
    it.method(c, "isFinite", |_, _, _, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Num(n) if n.is_finite()),
        ))
    });
    it.method(c, "isNaN", |_, _, _, args| {
        Ok(Value::Bool(
            matches!(arg(args, 0), Value::Num(n) if n.is_nan()),
        ))
    });
    let g = it.global;
    it.set_own(g, "NaN", Value::Num(f64::NAN));
    it.set_own(g, "Infinity", Value::Num(f64::INFINITY));
    let parse_float = it.native(|it, host, _, args| {
        let s = string(it, host, &arg(args, 0))?;
        let t = s.trim_start();
        let end = t
            .char_indices()
            .take_while(|&(i, c)| {
                c.is_ascii_digit()
                    || c == '.'
                    || (i == 0 && (c == '-' || c == '+'))
                    || c == 'e'
                    || c == 'E'
            })
            .map(|(i, c)| i + c.len_utf8())
            .last()
            .unwrap_or(0);
        let mut end = end;
        while end > 0 && t[..end].parse::<f64>().is_err() {
            end -= 1;
        }
        Ok(Value::Num(if t.starts_with("Infinity") {
            f64::INFINITY
        } else {
            t[..end].parse().unwrap_or(f64::NAN)
        }))
    });
    it.set_own(g, "parseFloat", Value::Obj(parse_float));
    it.set_own(c, "parseFloat", Value::Obj(parse_float));
    let parse_int = it.native(|it, host, _, args| {
        let s = string(it, host, &arg(args, 0))?;
        let mut t = s.trim();
        let neg = t.starts_with('-');
        t = t.trim_start_matches(['-', '+']);
        let mut radix = match arg(args, 1) {
            Value::Undefined => 10,
            v => it.to_number(host, &v)? as u32,
        };
        if radix == 0 {
            radix = 10;
        }
        if (radix == 16 || matches!(arg(args, 1), Value::Undefined))
            && (t.starts_with("0x") || t.starts_with("0X"))
        {
            t = &t[2..];
            radix = 16;
        }
        if !(2..=36).contains(&radix) {
            return Ok(Value::Num(f64::NAN));
        }
        let digits: String = t.chars().take_while(|c| c.is_digit(radix)).collect();
        if digits.is_empty() {
            return Ok(Value::Num(f64::NAN));
        }
        let v = digits.chars().fold(0f64, |acc, c| {
            acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
        });
        Ok(Value::Num(if neg { -v } else { v }))
    });
    it.set_own(g, "parseInt", Value::Obj(parse_int));
    it.set_own(c, "parseInt", Value::Obj(parse_int));
    it.method(g, "isNaN", |it, host, _, args| {
        Ok(Value::Bool(num(it, host, args, 0)?.is_nan()))
    });
    it.method(g, "isFinite", |it, host, _, args| {
        Ok(Value::Bool(num(it, host, args, 0)?.is_finite()))
    });
    it.method(p, "toFixed", |it, host, this, args| {
        let v = it.to_number(host, &this)?;
        let d = num(it, host, args, 0)?;
        let d = if d.is_nan() {
            0
        } else {
            d.clamp(0.0, 100.0) as usize
        };
        Ok(Value::str(&format!("{v:.d$}")))
    });
    it.method(p, "toString", |it, host, this, args| {
        let v = it.to_number(host, &this)?;
        let radix = match arg(args, 0) {
            Value::Undefined => 10,
            r => it.to_number(host, &r)? as u32,
        };
        if radix == 10 || !(2..=36).contains(&radix) || v.fract() != 0.0 || !v.is_finite() {
            return Ok(Value::str(&crate::interp::number_to_string(v)));
        }
        let (neg, mut n) = (v < 0.0, v.abs() as u64);
        let mut digits = Vec::new();
        loop {
            digits.push(std::char::from_digit((n % u64::from(radix)) as u32, radix).unwrap_or('0'));
            n /= u64::from(radix);
            if n == 0 {
                break;
            }
        }
        if neg {
            digits.push('-');
        }
        Ok(Value::str(&digits.iter().rev().collect::<String>()))
    });
    it.method(p, "valueOf", |it, host, this, _| {
        Ok(Value::Num(it.to_number(host, &this)?))
    });
    it.method(p, "toLocaleString", |it, host, this, _| {
        let v = it.to_number(host, &this)?;
        Ok(Value::str(&crate::interp::number_to_string(v)))
    });
}

fn install_errors(it: &mut Interp) {
    let p = it.protos.error;
    it.set_own(p, "name", Value::str("Error"));
    it.set_own(p, "message", Value::str(""));
    it.method(p, "toString", |it, host, this, _| {
        let n = it.get_value(host, &this, "name")?;
        let m = it.get_value(host, &this, "message")?;
        let (n, m) = (string(it, host, &n)?, string(it, host, &m)?);
        Ok(Value::str(&if m.is_empty() {
            n
        } else {
            format!("{n}: {m}")
        }))
    });
    macro_rules! error_type {
        ($name:literal) => {
            constructor(
                it,
                $name,
                |it, host, _, args| {
                    let m = match arg(args, 0) {
                        Value::Undefined => String::new(),
                        v => string(it, host, &v)?,
                    };
                    it.make_error($name, &m)
                },
                p,
            );
        };
    }
    error_type!("Error");
    // The other kinds share Error's prototype, so `e instanceof Error` holds for all.
    for (name, f) in [
        (
            "TypeError",
            (|it: &mut Interp, host: &mut dyn Host, _: Value, args: &[Value]| {
                let m = string(it, host, &arg(args, 0))?;
                it.make_error("TypeError", &m)
            }) as crate::interp::NativeFn,
        ),
        ("RangeError", |it, host, _, args| {
            let m = string(it, host, &arg(args, 0))?;
            it.make_error("RangeError", &m)
        }),
        ("SyntaxError", |it, host, _, args| {
            let m = string(it, host, &arg(args, 0))?;
            it.make_error("SyntaxError", &m)
        }),
        ("ReferenceError", |it, host, _, args| {
            let m = string(it, host, &arg(args, 0))?;
            it.make_error("ReferenceError", &m)
        }),
    ] {
        let c = it.native(f);
        it.set_own(c, "prototype", Value::Obj(p));
        let g = it.global;
        it.set_own(g, name, Value::Obj(c));
    }
}

fn install_math(it: &mut Interp) {
    let math = it.alloc(Some(it.protos.object), Kind::Plain).unwrap_or(0);
    let g = it.global;
    it.set_own(g, "Math", Value::Obj(math));
    it.set_own(math, "PI", Value::Num(std::f64::consts::PI));
    it.set_own(math, "E", Value::Num(std::f64::consts::E));
    it.set_own(math, "LN2", Value::Num(std::f64::consts::LN_2));
    it.set_own(math, "SQRT2", Value::Num(std::f64::consts::SQRT_2));
    macro_rules! unary {
        ($name:literal, $f:expr) => {
            it.method(math, $name, |it, host, _, args| {
                let x = num(it, host, args, 0)?;
                let f: fn(f64) -> f64 = $f;
                Ok(Value::Num(f(x)))
            });
        };
    }
    unary!("floor", f64::floor);
    unary!("ceil", f64::ceil);
    unary!("round", |x| (x + 0.5).floor());
    unary!("abs", f64::abs);
    unary!("sqrt", f64::sqrt);
    unary!("cbrt", f64::cbrt);
    unary!("trunc", f64::trunc);
    unary!("sign", |x| if x.is_nan() || x == 0.0 {
        x
    } else {
        x.signum()
    });
    unary!("sin", f64::sin);
    unary!("cos", f64::cos);
    unary!("tan", f64::tan);
    unary!("atan", f64::atan);
    unary!("asin", f64::asin);
    unary!("acos", f64::acos);
    unary!("exp", f64::exp);
    unary!("log", f64::ln);
    unary!("log2", f64::log2);
    unary!("log10", f64::log10);
    it.method(math, "atan2", |it, host, _, args| {
        Ok(Value::Num(
            num(it, host, args, 0)?.atan2(num(it, host, args, 1)?),
        ))
    });
    it.method(math, "pow", |it, host, _, args| {
        Ok(Value::Num(
            num(it, host, args, 0)?.powf(num(it, host, args, 1)?),
        ))
    });
    it.method(math, "hypot", |it, host, _, args| {
        let mut s = 0.0;
        for i in 0..args.len() {
            let x = num(it, host, args, i)?;
            s += x * x;
        }
        Ok(Value::Num(s.sqrt()))
    });
    it.method(math, "max", |it, host, _, args| {
        let mut m = f64::NEG_INFINITY;
        for i in 0..args.len() {
            let x = num(it, host, args, i)?;
            if x.is_nan() {
                return Ok(Value::Num(f64::NAN));
            }
            m = m.max(x);
        }
        Ok(Value::Num(m))
    });
    it.method(math, "min", |it, host, _, args| {
        let mut m = f64::INFINITY;
        for i in 0..args.len() {
            let x = num(it, host, args, i)?;
            if x.is_nan() {
                return Ok(Value::Num(f64::NAN));
            }
            m = m.min(x);
        }
        Ok(Value::Num(m))
    });
    it.method(math, "random", |it, _, _, _| {
        // xorshift seeded per page from the clock and the heap size; not for cryptography.
        let seed = it
            .get_own(it.global, "__seed")
            .map_or(0x9e37_79b9_7f4a_7c15u64, |v| match v {
                Value::Num(n) => n as u64,
                _ => 1,
            });
        let mut x = seed ^ (it.now.to_bits() >> 3) ^ 0x2545_f491;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let g = it.global;
        it.set_own(g, "__seed", Value::Num((x >> 11) as f64));
        Ok(Value::Num((x >> 11) as f64 / (1u64 << 53) as f64))
    });
}

fn install_json(it: &mut Interp) {
    let json = it.alloc(Some(it.protos.object), Kind::Plain).unwrap_or(0);
    let g = it.global;
    it.set_own(g, "JSON", Value::Obj(json));
    it.method(json, "stringify", |it, host, _, args| {
        let indent = match arg(args, 2) {
            Value::Num(n) => " ".repeat(n.clamp(0.0, 10.0) as usize),
            Value::Str(s) => s.chars().take(10).collect(),
            _ => String::new(),
        };
        let mut out = String::new();
        if stringify(it, host, &arg(args, 0), &indent, 0, &mut out)? {
            Ok(Value::str(&out))
        } else {
            Ok(Value::Undefined)
        }
    });
    it.method(json, "parse", |it, host, _, args| {
        let s = string(it, host, &arg(args, 0))?;
        let mut p = JsonParser {
            s: s.as_bytes(),
            i: 0,
        };
        let v = p.value(it)?;
        p.ws();
        if p.i != p.s.len() {
            return Err(it.throw_error("SyntaxError", "unexpected text after JSON"));
        }
        Ok(v)
    });
}

fn quote(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Writes `v` as JSON; false when it has no JSON form (undefined, functions).
fn stringify(
    it: &mut Interp,
    host: &mut dyn Host,
    v: &Value,
    indent: &str,
    depth: usize,
    out: &mut String,
) -> Result<bool, Ctrl> {
    if depth > 100 {
        return Err(it.throw_error("TypeError", "cyclic or too deep structure"));
    }
    let nl = |out: &mut String, d: usize| {
        if !indent.is_empty() {
            out.push('\n');
            for _ in 0..d {
                out.push_str(indent);
            }
        }
    };
    match v {
        Value::Undefined => return Ok(false),
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Num(n) => out.push_str(&if n.is_finite() {
            crate::interp::number_to_string(*n)
        } else {
            "null".into()
        }),
        Value::Str(s) => quote(s, out),
        Value::Obj(_) if it.is_callable(v) => return Ok(false),
        Value::Obj(o) => {
            if let Some(f) = it.get(host, v, "toJSON")? {
                if it.is_callable(&f) {
                    let r = it.call(host, &f, v.clone(), &[])?;
                    return stringify(it, host, &r, indent, depth, out);
                }
            }
            if let Some(items) = array_items(it, v) {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    nl(out, depth + 1);
                    if !stringify(it, host, item, indent, depth + 1, out)? {
                        out.push_str("null");
                    }
                }
                if !items.is_empty() {
                    nl(out, depth);
                }
                out.push(']');
            } else {
                let _ = o;
                out.push('{');
                let mut first = true;
                for k in it.keys(v) {
                    let item = it.get_value(host, v, &k)?;
                    let mark = out.len();
                    if !first {
                        out.push(',');
                    }
                    nl(out, depth + 1);
                    quote(&k, out);
                    out.push(':');
                    if !indent.is_empty() {
                        out.push(' ');
                    }
                    if stringify(it, host, &item, indent, depth + 1, out)? {
                        first = false;
                    } else {
                        out.truncate(mark);
                    }
                }
                if !first {
                    nl(out, depth);
                }
                out.push('}');
            }
        }
    }
    Ok(true)
}

struct JsonParser<'a> {
    s: &'a [u8],
    i: usize,
}

impl JsonParser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn fail<T>(&self, it: &mut Interp) -> Result<T, Ctrl> {
        Err(it.throw_error("SyntaxError", &format!("bad JSON at position {}", self.i)))
    }

    fn value(&mut self, it: &mut Interp) -> R {
        self.ws();
        match self.s.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                let o = it.new_object()?;
                self.ws();
                if self.s.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(o);
                }
                loop {
                    self.ws();
                    let Value::Str(k) = self.value(it)? else {
                        return self.fail(it);
                    };
                    self.ws();
                    if self.s.get(self.i) != Some(&b':') {
                        return self.fail(it);
                    }
                    self.i += 1;
                    let v = self.value(it)?;
                    if let Value::Obj(id) = o {
                        it.set_own(id, &k, v);
                    }
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(o);
                        }
                        _ => return self.fail(it),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.s.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return it.new_array(items);
                }
                loop {
                    items.push(self.value(it)?);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return it.new_array(items);
                        }
                        _ => return self.fail(it),
                    }
                }
            }
            Some(b'"') => {
                self.i += 1;
                let mut out = String::new();
                let rest = std::str::from_utf8(&self.s[self.i..]).unwrap_or("");
                let mut chars = rest.char_indices();
                while let Some((k, c)) = chars.next() {
                    match c {
                        '"' => {
                            self.i += k + 1;
                            return Ok(Value::str(&out));
                        }
                        '\\' => match chars.next().map(|c| c.1) {
                            Some('n') => out.push('\n'),
                            Some('t') => out.push('\t'),
                            Some('r') => out.push('\r'),
                            Some('b') => out.push('\u{8}'),
                            Some('f') => out.push('\u{c}'),
                            Some('u') => out.push(crate::lexer::unicode_escape(&mut chars)),
                            Some(other) => out.push(other),
                            None => break,
                        },
                        c => out.push(c),
                    }
                }
                self.fail(it)
            }
            Some(b't') if self.s[self.i..].starts_with(b"true") => {
                self.i += 4;
                Ok(Value::Bool(true))
            }
            Some(b'f') if self.s[self.i..].starts_with(b"false") => {
                self.i += 5;
                Ok(Value::Bool(false))
            }
            Some(b'n') if self.s[self.i..].starts_with(b"null") => {
                self.i += 4;
                Ok(Value::Null)
            }
            Some(c) if c.is_ascii_digit() || *c == b'-' => {
                let start = self.i;
                self.i += 1;
                while self.i < self.s.len()
                    && (self.s[self.i].is_ascii_digit() || b".eE+-".contains(&self.s[self.i]))
                {
                    self.i += 1;
                }
                let text = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("");
                match text.parse() {
                    Ok(v) => Ok(Value::Num(v)),
                    Err(_) => self.fail(it),
                }
            }
            _ => self.fail(it),
        }
    }
}

/// `Map` and `Set`: entries kept in an array, compared with `===` (fine for page scripts).
fn install_collections(it: &mut Interp) {
    fn entries(it: &mut Interp, host: &mut dyn Host, this: &Value) -> Result<ObjId, Ctrl> {
        match it.get(host, this, "__entries")? {
            Some(Value::Obj(o)) => Ok(o),
            _ => it.type_error("not a Map or Set"),
        }
    }
    fn position(it: &Interp, list: ObjId, key: &Value, pairs: bool) -> Option<usize> {
        let Kind::Array(items) = &it.objects[list as usize].kind else {
            return None;
        };
        items.iter().position(|e| {
            let k = if pairs {
                match e {
                    Value::Obj(p) => match &it.objects[*p as usize].kind {
                        Kind::Array(kv) => kv.first().cloned().unwrap_or(Value::Undefined),
                        _ => Value::Undefined,
                    },
                    _ => Value::Undefined,
                }
            } else {
                e.clone()
            };
            it.strict_equals(&k, key)
        })
    }
    fn items(it: &Interp, list: ObjId) -> Vec<Value> {
        match &it.objects[list as usize].kind {
            Kind::Array(v) => v.clone(),
            _ => Vec::new(),
        }
    }
    for (name, pairs) in [("Map", true), ("Set", false)] {
        let proto = it.alloc(Some(it.protos.object), Kind::Plain).unwrap_or(0);
        let make: crate::interp::NativeFn = if pairs {
            |it, host, _, args| {
                let proto = constructor_proto(it, "Map");
                let o = Value::Obj(it.alloc(Some(proto), Kind::Plain)?);
                let list = it.new_array(Vec::new())?;
                it.put(host, &o, "__entries", list)?;
                let init = arg(args, 0);
                if !init.is_nullish() {
                    let set = it.get_value(host, &o, "set")?;
                    for e in it.iterate(host, &init)? {
                        let kv = it.iterate(host, &e)?;
                        it.call(host, &set, o.clone(), &kv)?;
                    }
                }
                Ok(o)
            }
        } else {
            |it, host, _, args| {
                let proto = constructor_proto(it, "Set");
                let o = Value::Obj(it.alloc(Some(proto), Kind::Plain)?);
                let list = it.new_array(Vec::new())?;
                it.put(host, &o, "__entries", list)?;
                let init = arg(args, 0);
                if !init.is_nullish() {
                    let add = it.get_value(host, &o, "add")?;
                    for e in it.iterate(host, &init)? {
                        it.call(host, &add, o.clone(), &[e])?;
                    }
                }
                Ok(o)
            }
        };
        let c = constructor(it, name, make, proto);
        let f = it.native(make);
        it.set_own(c, "__construct", Value::Obj(f));
        it.method(
            proto,
            "has",
            if pairs {
                |it, host, this, args| {
                    let l = entries(it, host, &this)?;
                    Ok(Value::Bool(position(it, l, &arg(args, 0), true).is_some()))
                }
            } else {
                |it, host, this, args| {
                    let l = entries(it, host, &this)?;
                    Ok(Value::Bool(position(it, l, &arg(args, 0), false).is_some()))
                }
            },
        );
        it.method(
            proto,
            "delete",
            if pairs {
                |it, host, this, args| {
                    let l = entries(it, host, &this)?;
                    let found = position(it, l, &arg(args, 0), true);
                    if let (Some(i), Kind::Array(v)) = (found, &mut it.objects[l as usize].kind) {
                        v.remove(i);
                    }
                    Ok(Value::Bool(found.is_some()))
                }
            } else {
                |it, host, this, args| {
                    let l = entries(it, host, &this)?;
                    let found = position(it, l, &arg(args, 0), false);
                    if let (Some(i), Kind::Array(v)) = (found, &mut it.objects[l as usize].kind) {
                        v.remove(i);
                    }
                    Ok(Value::Bool(found.is_some()))
                }
            },
        );
        it.method(proto, "clear", |it, host, this, _| {
            let l = entries(it, host, &this)?;
            if let Kind::Array(v) = &mut it.objects[l as usize].kind {
                v.clear();
            }
            Ok(Value::Undefined)
        });
        it.method(
            proto,
            "forEach",
            if pairs {
                |it, host, this, args| {
                    let f = callback(it, args)?;
                    let l = entries(it, host, &this)?;
                    for e in items(it, l) {
                        let kv = it.iterate(host, &e)?;
                        it.call(
                            host,
                            &f,
                            Value::Undefined,
                            &[arg(&kv, 1), arg(&kv, 0), this.clone()],
                        )?;
                    }
                    Ok(Value::Undefined)
                }
            } else {
                |it, host, this, args| {
                    let f = callback(it, args)?;
                    let l = entries(it, host, &this)?;
                    for e in items(it, l) {
                        it.call(host, &f, Value::Undefined, &[e.clone(), e, this.clone()])?;
                    }
                    Ok(Value::Undefined)
                }
            },
        );
        if pairs {
            it.method(proto, "get", |it, host, this, args| {
                let l = entries(it, host, &this)?;
                Ok(match position(it, l, &arg(args, 0), true) {
                    Some(i) => {
                        let e = items(it, l)[i].clone();
                        it.get_value(host, &e, "1")?
                    }
                    None => Value::Undefined,
                })
            });
            it.method(proto, "set", |it, host, this, args| {
                let l = entries(it, host, &this)?;
                let pair = it.new_array(vec![arg(args, 0), arg(args, 1)])?;
                match position(it, l, &arg(args, 0), true) {
                    Some(i) => {
                        if let Kind::Array(v) = &mut it.objects[l as usize].kind {
                            v[i] = pair;
                        }
                    }
                    None => {
                        if let Kind::Array(v) = &mut it.objects[l as usize].kind {
                            v.push(pair);
                        }
                    }
                }
                Ok(this)
            });
            it.method(proto, "keys", |it, host, this, _| {
                let l = entries(it, host, &this)?;
                let mut out = Vec::new();
                for e in items(it, l) {
                    out.push(it.get_value(host, &e, "0")?);
                }
                it.new_array(out)
            });
            it.method(proto, "values", |it, host, this, _| {
                let l = entries(it, host, &this)?;
                let mut out = Vec::new();
                for e in items(it, l) {
                    out.push(it.get_value(host, &e, "1")?);
                }
                it.new_array(out)
            });
            it.method(proto, "entries", |it, host, this, _| {
                let l = entries(it, host, &this)?;
                let all = items(it, l);
                it.new_array(all)
            });
        } else {
            it.method(proto, "add", |it, host, this, args| {
                let l = entries(it, host, &this)?;
                if position(it, l, &arg(args, 0), false).is_none() {
                    if let Kind::Array(v) = &mut it.objects[l as usize].kind {
                        v.push(arg(args, 0));
                    }
                }
                Ok(this)
            });
            it.method(proto, "values", |it, host, this, _| {
                let l = entries(it, host, &this)?;
                let all = items(it, l);
                it.new_array(all)
            });
        }
        // `for…of` and spread: a Map gives its entries, a Set its values.
        let all = it.get_own(proto, if pairs { "entries" } else { "values" });
        if let Some(f) = all {
            it.set_own(proto, "@@iterator", f);
        }
    }
}

fn constructor_proto(it: &Interp, name: &str) -> ObjId {
    match it.get_own(it.global, name) {
        Some(Value::Obj(c)) => match it.get_own(c, "prototype") {
            Some(Value::Obj(p)) => p,
            _ => it.protos.object,
        },
        _ => it.protos.object,
    }
}

// ---------------------------------------------------------------------------------------
// The window: console, timers, location, storage.

pub fn inspect(it: &mut Interp, host: &mut dyn Host, v: &Value, depth: usize) -> String {
    match v {
        Value::Str(s) if depth == 0 => s.to_string(),
        Value::Str(s) => format!("{s:?}"),
        Value::Obj(o) => {
            let o = *o;
            if it.is_callable(v) {
                return "[function]".into();
            }
            if depth > 2 {
                return "…".into();
            }
            match &it.objects[o as usize].kind {
                Kind::Array(items) => {
                    let items = items.clone();
                    let parts: Vec<String> = items
                        .iter()
                        .take(100)
                        .map(|x| inspect(it, host, x, depth + 1))
                        .collect();
                    format!("[{}]", parts.join(", "))
                }
                Kind::Element(n) => format!("<{}>", host.tag(*n).unwrap_or_else(|| "#text".into())),
                Kind::Error => it.describe(host, v),
                Kind::RegExp(_) => {
                    let s = it
                        .get_own(o, "source")
                        .map(|s| inspect(it, host, &s, 0))
                        .unwrap_or_default();
                    format!("/{s}/")
                }
                _ => {
                    let props: Vec<(Arc<str>, Value)> = it.objects[o as usize]
                        .props
                        .iter()
                        .take(50)
                        .cloned()
                        .collect();
                    let parts: Vec<String> = props
                        .iter()
                        .filter(|(k, _)| !k.starts_with("__"))
                        .map(|(k, x)| format!("{k}: {}", inspect(it, host, x, depth + 1)))
                        .collect();
                    format!("{{{}}}", parts.join(", "))
                }
            }
        }
        other => it
            .to_string(host, other)
            .map(|s| s.to_string())
            .unwrap_or_default(),
    }
}

fn add_timer(it: &mut Interp, host: &mut dyn Host, args: &[Value], repeat: bool) -> R {
    let f = arg(args, 0);
    let ms = match arg(args, 1) {
        Value::Undefined => 0.0,
        v => it.to_number(host, &v)?,
    };
    let ms = if ms.is_nan() { 0.0 } else { ms.max(0.0) };
    if it.timers.len() >= 10_000 {
        return Err(Ctrl::Abort("too many timers".into()));
    }
    let id = it.next_timer;
    it.next_timer += 1;
    it.timers.push(Timer {
        id,
        due: it.now + ms,
        every: repeat.then_some(ms),
        callback: f,
        args: args.get(2..).map(<[Value]>::to_vec).unwrap_or_default(),
    });
    Ok(Value::Num(f64::from(id)))
}

fn install_window(it: &mut Interp) {
    let g = it.global;
    let console = it.alloc(Some(it.protos.object), Kind::Plain).unwrap_or(0);
    it.set_own(g, "console", Value::Obj(console));
    let log: crate::interp::NativeFn = |it, host, _, args| {
        let parts: Vec<String> = args.iter().map(|a| inspect(it, host, a, 0)).collect();
        host.log(&parts.join(" "));
        Ok(Value::Undefined)
    };
    for name in ["log", "info", "warn", "error", "debug", "trace"] {
        it.method(console, name, log);
    }
    for name in [
        "group", "groupEnd", "time", "timeEnd", "table", "dir", "assert", "count",
    ] {
        it.method(console, name, |_, _, _, _| Ok(Value::Undefined));
    }
    it.method(g, "setTimeout", |it, host, _, args| {
        add_timer(it, host, args, false)
    });
    it.method(g, "setInterval", |it, host, _, args| {
        add_timer(it, host, args, true)
    });
    it.method(g, "requestAnimationFrame", |it, host, _, args| {
        let f = arg(args, 0);
        let now = Value::Num(it.now);
        add_timer(it, host, &[f, Value::Num(16.0), now], false)
    });
    let clear: crate::interp::NativeFn = |it, host, _, args| {
        let id = num(it, host, args, 0)?;
        it.timers.retain(|t| f64::from(t.id) != id);
        Ok(Value::Undefined)
    };
    for name in ["clearTimeout", "clearInterval", "cancelAnimationFrame"] {
        it.method(g, name, clear);
    }
    it.method(g, "alert", |it, host, _, args| {
        let m = string(it, host, &arg(args, 0))?;
        host.log(&format!("alert: {m}"));
        Ok(Value::Undefined)
    });
    it.method(g, "confirm", |_, _, _, _| Ok(Value::Bool(false)));
    it.method(g, "prompt", |_, _, _, _| Ok(Value::Null));
    it.method(g, "addEventListener", |it, host, _, args| {
        let kind = it.to_string(host, &arg(args, 0))?;
        it.listeners.push((Target::Window, kind, arg(args, 1)));
        Ok(Value::Undefined)
    });
    it.method(g, "removeEventListener", |it, host, _, args| {
        let kind = it.to_string(host, &arg(args, 0))?;
        let f = arg(args, 1);
        it.listeners.retain(|(t, k, h)| {
            !(*t == Target::Window
                && *k == kind
                && matches!((h, &f), (Value::Obj(a), Value::Obj(b)) if a == b))
        });
        Ok(Value::Undefined)
    });
    it.method(g, "getComputedStyle", |it, _, _, args| {
        match node_of(it, &arg(args, 0)) {
            Some(n) => style_object(it, n),
            None => it.new_object(),
        }
    });
    it.method(g, "matchMedia", |it, host, _, _| {
        let o = it.new_object()?;
        it.put(host, &o, "matches", Value::Bool(false))?;
        if let Value::Obj(id) = o {
            for name in [
                "addListener",
                "removeListener",
                "addEventListener",
                "removeEventListener",
            ] {
                it.method(id, name, |_, _, _, _| Ok(Value::Undefined));
            }
        }
        Ok(o)
    });
    it.method(g, "encodeURIComponent", |it, host, _, args| {
        let s = string(it, host, &arg(args, 0))?;
        Ok(Value::str(&encode(&s, b"-_.!~*'()")))
    });
    it.method(g, "encodeURI", |it, host, _, args| {
        let s = string(it, host, &arg(args, 0))?;
        Ok(Value::str(&encode(&s, b"-_.!~*'();/?:@&=+$,#")))
    });
    let decode: crate::interp::NativeFn = |it, host, _, args| {
        let s = string(it, host, &arg(args, 0))?;
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                if let Ok(v) =
                    u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
                {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
            out.push(b[i]);
            i += 1;
        }
        match String::from_utf8(out) {
            Ok(s) => Ok(Value::str(&s)),
            Err(_) => Err(it.throw_error("URIError", "malformed URI sequence")),
        }
    };
    it.method(g, "decodeURIComponent", decode);
    it.method(g, "decodeURI", decode);

    let date = it.alloc(Some(it.protos.object), Kind::Plain).unwrap_or(0);
    let c = constructor(
        it,
        "Date",
        |it, host, _, args| {
            let proto = constructor_proto(it, "Date");
            let o = Value::Obj(it.alloc(Some(proto), Kind::Plain)?);
            let t = match arg(args, 0) {
                Value::Num(n) => n,
                Value::Undefined => it.epoch_ms + it.now,
                _ => f64::NAN,
            };
            it.put(host, &o, "__time", Value::Num(t))?;
            Ok(o)
        },
        date,
    );
    let f = it.native(|it, host, this, args| {
        let ctor = it.get_own(it.global, "Date").unwrap_or(Value::Undefined);
        let _ = this;
        let Value::Obj(c) = ctor else {
            return it.new_object();
        };
        let make = it.get_own(c, "__make").unwrap_or(Value::Undefined);
        it.call(host, &make, Value::Undefined, args)
    });
    let make = it.native(|it, host, _, args| {
        let proto = constructor_proto(it, "Date");
        let o = Value::Obj(it.alloc(Some(proto), Kind::Plain)?);
        let t = match arg(args, 0) {
            Value::Num(n) => n,
            Value::Undefined => it.epoch_ms + it.now,
            _ => f64::NAN,
        };
        it.put(host, &o, "__time", Value::Num(t))?;
        Ok(o)
    });
    it.set_own(c, "__make", Value::Obj(make));
    it.set_own(c, "__construct", Value::Obj(f));
    it.method(c, "now", |it, _, _, _| {
        Ok(Value::Num((it.epoch_ms + it.now).floor()))
    });
    let time: crate::interp::NativeFn = |it, host, this, _| it.get_value(host, &this, "__time");
    it.method(date, "getTime", time);
    it.method(date, "valueOf", time);
    it.method(date, "getFullYear", |it, host, this, _| {
        let t = it.get_value(host, &this, "__time")?;
        let t = it.to_number(host, &t)?;
        Ok(Value::Num((1970.0 + t / 31_556_952_000.0).floor()))
    });
    it.method(date, "toISOString", |it, host, this, _| {
        let t = it.get_value(host, &this, "__time")?;
        let t = it.to_number(host, &t)?;
        Ok(Value::str(&iso_date(t)))
    });
    it.method(date, "toString", |it, host, this, _| {
        let t = it.get_value(host, &this, "__time")?;
        let t = it.to_number(host, &t)?;
        Ok(Value::str(&iso_date(t)))
    });

    let perf = it.alloc(Some(it.protos.object), Kind::Plain).unwrap_or(0);
    it.set_own(g, "performance", Value::Obj(perf));
    it.method(perf, "now", |it, _, _, _| Ok(Value::Num(it.now)));

    let navigator = it.alloc(Some(it.protos.object), Kind::Plain).unwrap_or(0);
    it.set_own(g, "navigator", Value::Obj(navigator));
    it.set_own(navigator, "userAgent", Value::str("Mozilla/5.0 Broxee"));
    it.set_own(navigator, "language", Value::str("en-US"));
    it.set_own(navigator, "onLine", Value::Bool(true));
    it.set_own(navigator, "cookieEnabled", Value::Bool(false));

    // Storage that lives as long as the page: nothing is written to disk.
    for name in ["localStorage", "sessionStorage"] {
        let s = it.alloc(Some(it.protos.object), Kind::Plain).unwrap_or(0);
        it.set_own(g, name, Value::Obj(s));
        it.method(s, "getItem", |it, host, _, args| {
            let k = string(it, host, &arg(args, 0))?;
            Ok(it
                .storage
                .iter()
                .find(|(a, _)| *a == k)
                .map_or(Value::Null, |(_, v)| Value::str(v)))
        });
        it.method(s, "setItem", |it, host, _, args| {
            let k = string(it, host, &arg(args, 0))?;
            let v = string(it, host, &arg(args, 1))?;
            if it
                .storage
                .iter()
                .map(|(a, b)| a.len() + b.len())
                .sum::<usize>()
                + k.len()
                + v.len()
                > 1 << 20
            {
                return Err(it.throw_error("QuotaExceededError", "storage is full"));
            }
            it.storage.retain(|(a, _)| *a != k);
            it.storage.push((k, v));
            Ok(Value::Undefined)
        });
        it.method(s, "removeItem", |it, host, _, args| {
            let k = string(it, host, &arg(args, 0))?;
            it.storage.retain(|(a, _)| *a != k);
            Ok(Value::Undefined)
        });
        it.method(s, "clear", |it, _, _, _| {
            it.storage.clear();
            Ok(Value::Undefined)
        });
    }
}

fn iso_date(t: f64) -> String {
    if !t.is_finite() {
        return "Invalid Date".into();
    }
    let ms = t.rem_euclid(1000.0) as u64;
    let secs = (t / 1000.0).floor() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from days since 1970 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

fn encode(s: &str, keep: &[u8]) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || keep.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// The DOM.

fn style_object(it: &mut Interp, node: u32) -> R {
    let p = it.protos.style;
    Ok(Value::Obj(it.alloc(Some(p), Kind::Style(node))?))
}

/// `fontSize` → `font-size`; `cssFloat` → `float`.
fn css_name(key: &str) -> String {
    if key == "cssFloat" {
        return "float".into();
    }
    let mut out = String::new();
    for c in key.chars() {
        if c.is_ascii_uppercase() {
            out.push('-');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn declarations(style: &str) -> Vec<(String, String)> {
    style
        .split(';')
        .filter_map(|d| {
            let (k, v) = d.split_once(':')?;
            Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
        })
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

pub fn style_get(host: &mut dyn Host, node: u32, key: &str) -> String {
    let name = css_name(key);
    let style = host.attr(node, "style").unwrap_or_default();
    declarations(&style)
        .into_iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
        .unwrap_or_default()
}

pub fn style_set(host: &mut dyn Host, node: u32, key: &str, value: &str) {
    let name = if key.contains('-') {
        key.to_string()
    } else {
        css_name(key)
    };
    let style = host.attr(node, "style").unwrap_or_default();
    let mut decls: Vec<(String, String)> = declarations(&style)
        .into_iter()
        .filter(|(k, _)| *k != name)
        .collect();
    if !value.trim().is_empty() {
        decls.push((name, value.trim().to_string()));
    }
    let text: Vec<String> = decls.iter().map(|(k, v)| format!("{k}: {v}")).collect();
    host.set_attr(node, "style", &text.join("; "));
}

fn classes(host: &dyn Host, node: u32) -> Vec<String> {
    host.attr(node, "class")
        .unwrap_or_default()
        .split_ascii_whitespace()
        .map(str::to_string)
        .collect()
}

fn element_children(host: &dyn Host, node: u32) -> Vec<u32> {
    host.children(node)
        .into_iter()
        .filter(|&c| host.tag(c).is_some())
        .collect()
}

fn find_document_child(host: &dyn Host, tag: &str) -> Option<u32> {
    let html = element_children(host, 0)
        .into_iter()
        .find(|&c| host.tag(c).as_deref() == Some("html"))?;
    if tag == "html" {
        return Some(html);
    }
    element_children(host, html)
        .into_iter()
        .find(|&c| host.tag(c).as_deref() == Some(tag))
}

/// Properties of element objects that read the DOM. `None` falls through to the object's
/// own properties and the element prototype (methods).
pub fn element_get(
    it: &mut Interp,
    host: &mut dyn Host,
    n: u32,
    key: &str,
) -> Result<Option<Value>, Ctrl> {
    let attr = |host: &dyn Host, name: &str| {
        host.attr(n, name)
            .map_or(Value::str(""), |v| Value::str(&v))
    };
    let opt =
        |v: Option<u32>, it: &mut Interp| -> R { v.map_or(Ok(Value::Null), |n| element(it, n)) };
    Ok(Some(match key {
        "title" if n == 0 => {
            let t = host
                .select(None, "title")
                .first()
                .map(|&t| host.text(t))
                .unwrap_or_default();
            Value::str(t.trim())
        }
        "textContent" | "innerText" | "nodeValue" | "data" => Value::str(&host.text(n)),
        "innerHTML" | "outerHTML" => Value::str(&host.text(n)),
        "id" | "title" | "lang" | "dir" | "name" | "placeholder" | "alt" | "rel" | "target" => {
            attr(host, key)
        }
        "href" | "src" | "action" => attr(host, key),
        "type" => attr(host, "type"),
        "className" => attr(host, "class"),
        "tagName" | "nodeName" => match host.tag(n) {
            Some(t) => Value::str(&t.to_ascii_uppercase()),
            None if n == 0 => Value::str("#document"),
            None => Value::str("#text"),
        },
        "localName" => host.tag(n).map_or(Value::Null, |t| Value::str(&t)),
        "nodeType" => Value::Num(if n == 0 {
            9.0
        } else if host.is_text(n) {
            3.0
        } else {
            1.0
        }),
        "parentNode" => opt(host.parent(n), it)?,
        "parentElement" => opt(host.parent(n).filter(|&p| host.tag(p).is_some()), it)?,
        "children" => elements(it, element_children(host, n))?,
        "childNodes" => elements(it, host.children(n))?,
        "childElementCount" => Value::Num(element_children(host, n).len() as f64),
        "firstChild" => opt(host.children(n).first().copied(), it)?,
        "lastChild" => opt(host.children(n).last().copied(), it)?,
        "firstElementChild" => opt(element_children(host, n).first().copied(), it)?,
        "lastElementChild" => opt(element_children(host, n).last().copied(), it)?,
        "nextElementSibling" | "previousElementSibling" | "nextSibling" | "previousSibling" => {
            let Some(p) = host.parent(n) else {
                return Ok(Some(Value::Null));
            };
            let elements_only = key.contains("Element");
            let siblings: Vec<u32> = host
                .children(p)
                .into_iter()
                .filter(|&c| !elements_only || host.tag(c).is_some() || c == n)
                .collect();
            let i = siblings.iter().position(|&c| c == n).unwrap_or(0);
            let next = if key.starts_with("next") {
                siblings.get(i + 1)
            } else {
                i.checked_sub(1).and_then(|j| siblings.get(j))
            };
            opt(next.copied(), it)?
        }
        "classList" => {
            let p = it.protos.class_list;
            Value::Obj(it.alloc(Some(p), Kind::ClassList(n))?)
        }
        "style" => style_object(it, n)?,
        "dataset" => {
            let o = it.new_object()?;
            for (name, value) in data_attributes(host, n) {
                it.put(host, &o, &name, Value::str(&value))?;
            }
            o
        }
        "value" => match host.value(n) {
            Some(v) => Value::str(&v),
            None => attr(host, "value"),
        },
        "checked" => Value::Bool(host.checked(n).unwrap_or(false)),
        "disabled" | "hidden" | "required" | "readOnly" | "multiple" | "selected" => {
            Value::Bool(host.attr(n, &key.to_ascii_lowercase()).is_some())
        }
        "isConnected" => {
            let mut at = Some(n);
            while let Some(x) = at {
                if x == 0 {
                    return Ok(Some(Value::Bool(true)));
                }
                at = host.parent(x);
            }
            Value::Bool(false)
        }
        "offsetWidth" | "offsetHeight" | "clientWidth" | "clientHeight" | "scrollTop"
        | "scrollLeft" | "offsetTop" | "offsetLeft" | "scrollHeight" | "scrollWidth" => {
            Value::Num(0.0)
        }
        "ownerDocument" => element(it, 0)?,
        // The document's own properties.
        "body" | "head" | "documentElement" if n == 0 => {
            let tag = match key {
                "documentElement" => "html",
                t => t,
            };
            opt(find_document_child(host, tag), it)?
        }
        "readyState" if n == 0 => Value::str("complete"),
        "cookie" if n == 0 => Value::str(&host.cookie()),
        "URL" | "documentURI" if n == 0 => Value::str(&host.url()),
        "location" if n == 0 => it.get_value(host, &Value::Obj(it.global), "location")?,
        "defaultView" if n == 0 => Value::Obj(it.global),
        _ if key.starts_with("on") && key.len() > 2 => {
            let o = it.elements.get(&n).copied();
            match o.and_then(|o| it.get_own(o, &format!("__{key}"))) {
                Some(v) => v,
                None => Value::Null,
            }
        }
        _ => return Ok(None),
    }))
}

fn data_attributes(host: &dyn Host, n: u32) -> Vec<(String, String)> {
    // The host has no attribute listing; scripts read the common ones by name instead.
    let _ = (host, n);
    Vec::new()
}

/// Property writes that change the DOM. Returns false to store a plain property instead.
pub fn element_set(
    it: &mut Interp,
    host: &mut dyn Host,
    n: u32,
    key: &str,
    value: &Value,
) -> Result<bool, Ctrl> {
    match key {
        "cookie" if n == 0 => {
            let s = string(it, host, value)?;
            host.set_cookie(&s);
        }
        "textContent" | "innerText" | "nodeValue" | "data" => {
            let s = if value.is_nullish() {
                String::new()
            } else {
                string(it, host, value)?
            };
            host.set_text(n, &s);
        }
        "innerHTML" => {
            let s = string(it, host, value)?;
            host.set_html(n, &s);
        }
        "id" | "title" | "lang" | "dir" | "name" | "placeholder" | "alt" | "href" | "src"
        | "type" | "rel" | "target" | "action" => {
            let s = string(it, host, value)?;
            if n == 0 && key == "title" {
                if let Some(&t) = host.select(None, "title").first() {
                    host.set_text(t, &s);
                }
            } else {
                host.set_attr(n, key, &s);
            }
        }
        "className" => {
            let s = string(it, host, value)?;
            host.set_attr(n, "class", &s);
        }
        "value" => {
            let s = string(it, host, value)?;
            if host.value(n).is_some() {
                host.set_value(n, &s);
            } else {
                host.set_attr(n, "value", &s);
            }
        }
        "checked" => {
            let b = it.truthy(value);
            host.set_checked(n, b);
        }
        "disabled" | "hidden" | "required" | "readOnly" | "selected" => {
            let attr = key.to_ascii_lowercase();
            if it.truthy(value) {
                host.set_attr(n, &attr, "");
            } else {
                host.remove_attr(n, &attr);
            }
        }
        "style" => {
            let s = string(it, host, value)?;
            host.set_attr(n, "style", &s);
        }
        _ if key.starts_with("on") && key.len() > 2 => {
            let o = match element(it, n)? {
                Value::Obj(o) => o,
                _ => return Ok(false),
            };
            it.set_own(o, &format!("__{key}"), value.clone());
        }
        "location" if n == 0 => {}
        _ => return Ok(false),
    }
    Ok(true)
}

fn this_node(it: &mut Interp, this: &Value) -> Result<u32, Ctrl> {
    match node_of(it, this) {
        Some(n) => Ok(n),
        None => it.type_error("not a DOM node"),
    }
}

fn install_dom(it: &mut Interp) {
    let g = it.global;
    let document = element(it, 0).unwrap_or(Value::Undefined);
    it.set_own(g, "document", document);
    let location = it.alloc(Some(it.protos.object), Kind::Plain).unwrap_or(0);
    it.set_own(g, "location", Value::Obj(location));
    it.method(location, "toString", |_, host, _, _| {
        Ok(Value::str(&host.url()))
    });
    it.set_own(location, "__location", Value::Bool(true));
    for name in ["reload", "assign", "replace"] {
        it.method(location, name, |_, host, _, _| {
            host.log("navigation from scripts is not supported");
            Ok(Value::Undefined)
        });
    }
    for name in [
        "Node",
        "Element",
        "HTMLElement",
        "Document",
        "Event",
        "EventTarget",
    ] {
        let p = if name == "Event" {
            it.protos.event
        } else {
            it.protos.element
        };
        let c = it.native(|it, _, _, _| it.type_error("Illegal constructor"));
        it.set_own(c, "prototype", Value::Obj(p));
        it.set_own(g, name, Value::Obj(c));
    }
    let e = it.protos.element;
    it.method(e, "getAttribute", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let k = string(it, host, &arg(args, 0))?;
        Ok(host
            .attr(n, &k.to_ascii_lowercase())
            .map_or(Value::Null, |v| Value::str(&v)))
    });
    it.method(e, "hasAttribute", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let k = string(it, host, &arg(args, 0))?;
        Ok(Value::Bool(host.attr(n, &k.to_ascii_lowercase()).is_some()))
    });
    it.method(e, "setAttribute", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let k = string(it, host, &arg(args, 0))?;
        let v = string(it, host, &arg(args, 1))?;
        host.set_attr(n, &k.to_ascii_lowercase(), &v);
        Ok(Value::Undefined)
    });
    it.method(e, "removeAttribute", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let k = string(it, host, &arg(args, 0))?;
        host.remove_attr(n, &k.to_ascii_lowercase());
        Ok(Value::Undefined)
    });
    it.method(e, "toggleAttribute", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let k = string(it, host, &arg(args, 0))?.to_ascii_lowercase();
        let on = match arg(args, 1) {
            Value::Undefined => host.attr(n, &k).is_none(),
            v => it.truthy(&v),
        };
        if on {
            host.set_attr(n, &k, "");
        } else {
            host.remove_attr(n, &k);
        }
        Ok(Value::Bool(on))
    });
    it.method(e, "addEventListener", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let kind = it.to_string(host, &arg(args, 0))?;
        let f = arg(args, 1);
        if it.is_callable(&f) || matches!(f, Value::Obj(_)) {
            it.listeners.push((Target::Node(n), kind, f));
        }
        Ok(Value::Undefined)
    });
    it.method(e, "removeEventListener", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let kind = it.to_string(host, &arg(args, 0))?;
        let f = arg(args, 1);
        it.listeners.retain(|(t, k, h)| {
            !(*t == Target::Node(n)
                && *k == kind
                && matches!((h, &f), (Value::Obj(a), Value::Obj(b)) if a == b))
        });
        Ok(Value::Undefined)
    });
    it.method(e, "dispatchEvent", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let ev = arg(args, 0);
        let kind = it.get_value(host, &ev, "type")?;
        let kind = string(it, host, &kind)?;
        let prevented = dispatch(it, host, n, &kind)?;
        Ok(Value::Bool(!prevented))
    });
    it.method(e, "querySelector", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let sel = string(it, host, &arg(args, 0))?;
        let found = host.select(Some(n), &sel).first().copied();
        found.map_or(Ok(Value::Null), |f| element(it, f))
    });
    it.method(e, "querySelectorAll", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let sel = string(it, host, &arg(args, 0))?;
        let found = host.select(Some(n), &sel);
        elements(it, found)
    });
    it.method(e, "getElementById", |it, host, _, args| {
        let id = string(it, host, &arg(args, 0))?;
        let found = host
            .select(None, "*")
            .into_iter()
            .find(|&x| host.attr(x, "id").as_deref() == Some(id.as_str()));
        found.map_or(Ok(Value::Null), |f| element(it, f))
    });
    it.method(e, "getElementsByClassName", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let names = string(it, host, &arg(args, 0))?;
        let sel: String = names
            .split_ascii_whitespace()
            .map(|c| format!(".{c}"))
            .collect();
        let found = if sel.is_empty() {
            Vec::new()
        } else {
            host.select(Some(n), &sel)
        };
        elements(it, found)
    });
    it.method(e, "getElementsByTagName", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let tag = string(it, host, &arg(args, 0))?;
        let found = host.select(Some(n), &tag);
        elements(it, found)
    });
    it.method(e, "getElementsByName", |it, host, _, args| {
        let name = string(it, host, &arg(args, 0))?;
        let found = host
            .select(None, "*")
            .into_iter()
            .filter(|&x| host.attr(x, "name").as_deref() == Some(name.as_str()))
            .collect();
        elements(it, found)
    });
    it.method(e, "matches", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let sel = string(it, host, &arg(args, 0))?;
        Ok(Value::Bool(host.matches(n, &sel)))
    });
    it.method(e, "closest", |it, host, this, args| {
        let mut at = Some(this_node(it, &this)?);
        let sel = string(it, host, &arg(args, 0))?;
        while let Some(n) = at {
            if host.tag(n).is_some() && host.matches(n, &sel) {
                return element(it, n);
            }
            at = host.parent(n);
        }
        Ok(Value::Null)
    });
    it.method(e, "contains", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let mut at = node_of(it, &arg(args, 0));
        while let Some(x) = at {
            if x == n {
                return Ok(Value::Bool(true));
            }
            at = host.parent(x);
        }
        Ok(Value::Bool(false))
    });
    it.method(e, "appendChild", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let child = arg(args, 0);
        let c = this_node(it, &child)?;
        if !host.append(n, c) {
            return Err(it.throw_error("HierarchyRequestError", "cannot insert a node into itself"));
        }
        Ok(child)
    });
    it.method(e, "append", |it, host, this, args| {
        let n = this_node(it, &this)?;
        for a in args {
            let c = match node_of(it, a) {
                Some(c) => c,
                None => {
                    let s = string(it, host, a)?;
                    host.create_text(&s)
                }
            };
            host.append(n, c);
        }
        Ok(Value::Undefined)
    });
    it.method(e, "prepend", |it, host, this, args| {
        let n = this_node(it, &this)?;
        for a in args.iter().rev() {
            let c = match node_of(it, a) {
                Some(c) => c,
                None => {
                    let s = string(it, host, a)?;
                    host.create_text(&s)
                }
            };
            match host.children(n).first().copied() {
                Some(first) => host.insert_before(n, c, first),
                None => host.append(n, c),
            };
        }
        Ok(Value::Undefined)
    });
    it.method(e, "insertAdjacentHTML", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let at = string(it, host, &arg(args, 0))?.to_ascii_lowercase();
        let markup = string(it, host, &arg(args, 1))?;
        // Parse into a holder, then move its children into place.
        let holder = host.create_element("div");
        host.set_html(holder, &markup);
        let parts = host.children(holder);
        for c in parts {
            match at.as_str() {
                "beforeend" => {
                    host.append(n, c);
                }
                "afterbegin" => {
                    match host.children(n).first().copied() {
                        Some(first) => host.insert_before(n, c, first),
                        None => host.append(n, c),
                    };
                }
                "beforebegin" | "afterend" => {
                    let Some(parent) = host.parent(n) else { break };
                    let siblings = host.children(parent);
                    let i = siblings.iter().position(|&x| x == n).unwrap_or(0);
                    let before = if at == "beforebegin" {
                        Some(n)
                    } else {
                        siblings.get(i + 1).copied()
                    };
                    match before {
                        Some(b) => host.insert_before(parent, c, b),
                        None => host.append(parent, c),
                    };
                }
                _ => return Err(it.throw_error("SyntaxError", "insertAdjacentHTML: bad position")),
            }
        }
        Ok(Value::Undefined)
    });
    it.method(e, "insertBefore", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let child = arg(args, 0);
        let c = this_node(it, &child)?;
        match node_of(it, &arg(args, 1)) {
            Some(before) => host.insert_before(n, c, before),
            None => host.append(n, c),
        };
        Ok(child)
    });
    it.method(e, "removeChild", |it, host, this, args| {
        let _ = this_node(it, &this)?;
        let child = arg(args, 0);
        let c = this_node(it, &child)?;
        host.remove(c);
        Ok(child)
    });
    it.method(e, "replaceChild", |it, host, this, args| {
        let n = this_node(it, &this)?;
        let new = this_node(it, &arg(args, 0))?;
        let old = this_node(it, &arg(args, 1))?;
        host.insert_before(n, new, old);
        host.remove(old);
        Ok(arg(args, 1))
    });
    it.method(e, "remove", |it, host, this, _| {
        let n = this_node(it, &this)?;
        host.remove(n);
        Ok(Value::Undefined)
    });
    it.method(e, "replaceChildren", |it, host, this, args| {
        let n = this_node(it, &this)?;
        for c in host.children(n) {
            host.remove(c);
        }
        for a in args {
            if let Some(c) = node_of(it, a) {
                host.append(n, c);
            }
        }
        Ok(Value::Undefined)
    });
    it.method(e, "cloneNode", |it, host, this, _| {
        let n = this_node(it, &this)?;
        let tag = host.tag(n).unwrap_or_else(|| "span".into());
        let c = host.create_element(&tag);
        for name in [
            "id", "class", "style", "href", "src", "type", "name", "value",
        ] {
            if let Some(v) = host.attr(n, name) {
                host.set_attr(c, name, &v);
            }
        }
        let t = host.text(n);
        host.set_text(c, &t);
        element(it, c)
    });
    it.method(e, "createElement", |it, host, _, args| {
        let tag = string(it, host, &arg(args, 0))?.to_ascii_lowercase();
        let n = host.create_element(&tag);
        element(it, n)
    });
    it.method(e, "createTextNode", |it, host, _, args| {
        let s = string(it, host, &arg(args, 0))?;
        let n = host.create_text(&s);
        element(it, n)
    });
    it.method(e, "createDocumentFragment", |it, host, _, _| {
        let n = host.create_element("div");
        element(it, n)
    });
    it.method(e, "click", |it, host, this, _| {
        let n = this_node(it, &this)?;
        dispatch(it, host, n, "click")?;
        Ok(Value::Undefined)
    });
    for name in ["focus", "blur", "scrollIntoView", "scrollTo", "select"] {
        it.method(e, name, |_, _, _, _| Ok(Value::Undefined));
    }
    it.method(e, "getBoundingClientRect", |it, host, _, _| {
        let o = it.new_object()?;
        for k in [
            "x", "y", "top", "left", "right", "bottom", "width", "height",
        ] {
            it.put(host, &o, k, Value::Num(0.0))?;
        }
        Ok(o)
    });
    it.method(e, "toString", |it, host, this, _| {
        let n = this_node(it, &this)?;
        Ok(Value::str(&format!(
            "[object HTML{}Element]",
            host.tag(n).unwrap_or_default()
        )))
    });

    let cl = it.protos.class_list;
    fn list_node(it: &mut Interp, this: &Value) -> Result<u32, Ctrl> {
        match this {
            Value::Obj(o) => match it.objects[*o as usize].kind {
                Kind::ClassList(n) => Ok(n),
                _ => it.type_error("not a classList"),
            },
            _ => it.type_error("not a classList"),
        }
    }
    it.method(cl, "add", |it, host, this, args| {
        let n = list_node(it, &this)?;
        let mut c = classes(host, n);
        for a in args {
            let name = string(it, host, a)?;
            if !c.contains(&name) {
                c.push(name);
            }
        }
        host.set_attr(n, "class", &c.join(" "));
        Ok(Value::Undefined)
    });
    it.method(cl, "remove", |it, host, this, args| {
        let n = list_node(it, &this)?;
        let mut c = classes(host, n);
        for a in args {
            let name = string(it, host, a)?;
            c.retain(|x| *x != name);
        }
        host.set_attr(n, "class", &c.join(" "));
        Ok(Value::Undefined)
    });
    it.method(cl, "toggle", |it, host, this, args| {
        let n = list_node(it, &this)?;
        let mut c = classes(host, n);
        let name = string(it, host, &arg(args, 0))?;
        let on = match arg(args, 1) {
            Value::Undefined => !c.contains(&name),
            v => it.truthy(&v),
        };
        c.retain(|x| *x != name);
        if on {
            c.push(name);
        }
        host.set_attr(n, "class", &c.join(" "));
        Ok(Value::Bool(on))
    });
    it.method(cl, "contains", |it, host, this, args| {
        let n = list_node(it, &this)?;
        let name = string(it, host, &arg(args, 0))?;
        Ok(Value::Bool(classes(host, n).contains(&name)))
    });
    it.method(cl, "replace", |it, host, this, args| {
        let n = list_node(it, &this)?;
        let (old, new) = (
            string(it, host, &arg(args, 0))?,
            string(it, host, &arg(args, 1))?,
        );
        let c: Vec<String> = classes(host, n)
            .into_iter()
            .map(|x| if x == old { new.clone() } else { x })
            .collect();
        host.set_attr(n, "class", &c.join(" "));
        Ok(Value::Undefined)
    });
    it.method(cl, "item", |it, host, this, args| {
        let n = list_node(it, &this)?;
        let i = num(it, host, args, 0)? as usize;
        Ok(classes(host, n)
            .get(i)
            .map_or(Value::Null, |c| Value::str(c)))
    });

    let st = it.protos.style;
    it.method(st, "setProperty", |it, host, this, args| {
        let Value::Obj(o) = this else {
            return Ok(Value::Undefined);
        };
        let Kind::Style(n) = it.objects[o as usize].kind else {
            return Ok(Value::Undefined);
        };
        let (k, v) = (
            string(it, host, &arg(args, 0))?,
            string(it, host, &arg(args, 1))?,
        );
        style_set(host, n, &k, &v);
        Ok(Value::Undefined)
    });
    it.method(st, "removeProperty", |it, host, this, args| {
        let Value::Obj(o) = this else {
            return Ok(Value::Undefined);
        };
        let Kind::Style(n) = it.objects[o as usize].kind else {
            return Ok(Value::Undefined);
        };
        let k = string(it, host, &arg(args, 0))?;
        style_set(host, n, &k, "");
        Ok(Value::Undefined)
    });
    it.method(st, "getPropertyValue", |it, host, this, args| {
        let Value::Obj(o) = this else {
            return Ok(Value::str(""));
        };
        let Kind::Style(n) = it.objects[o as usize].kind else {
            return Ok(Value::str(""));
        };
        let k = string(it, host, &arg(args, 0))?;
        Ok(Value::str(&style_get(host, n, &k)))
    });

    let ev = it.protos.event;
    it.method(ev, "preventDefault", |it, host, this, _| {
        it.put(host, &this, "defaultPrevented", Value::Bool(true))?;
        Ok(Value::Undefined)
    });
    it.method(ev, "stopPropagation", |it, host, this, _| {
        it.put(host, &this, "__stop", Value::Bool(true))?;
        Ok(Value::Undefined)
    });
    it.method(ev, "stopImmediatePropagation", |it, host, this, _| {
        it.put(host, &this, "__stop", Value::Bool(true))?;
        Ok(Value::Undefined)
    });
}

fn event_object(it: &mut Interp, host: &mut dyn Host, kind: &str, target: Value) -> R {
    let p = it.protos.event;
    let e = Value::Obj(it.alloc(Some(p), Kind::Plain)?);
    it.put(host, &e, "type", Value::str(kind))?;
    it.put(host, &e, "target", target.clone())?;
    it.put(host, &e, "srcElement", target)?;
    it.put(host, &e, "defaultPrevented", Value::Bool(false))?;
    it.put(host, &e, "bubbles", Value::Bool(true))?;
    it.put(host, &e, "button", Value::Num(0.0))?;
    it.put(host, &e, "timeStamp", Value::Num(it.now))?;
    Ok(e)
}

fn call_listener(it: &mut Interp, host: &mut dyn Host, f: &Value, this: Value, event: &Value) -> R {
    if it.is_callable(f) {
        return it.call(host, f, this, std::slice::from_ref(event));
    }
    // An object with a `handleEvent` method.
    let h = it.get_value(host, f, "handleEvent")?;
    if it.is_callable(&h) {
        return it.call(host, &h, f.clone(), std::slice::from_ref(event));
    }
    Ok(Value::Undefined)
}

/// Fire `kind` at `node` and up through its ancestors, then the window.
pub fn dispatch(it: &mut Interp, host: &mut dyn Host, node: u32, kind: &str) -> Result<bool, Ctrl> {
    let target = element(it, node)?;
    let event = event_object(it, host, kind, target)?;
    let mut path = vec![node];
    let mut at = host.parent(node);
    while let Some(p) = at {
        path.push(p);
        at = host.parent(p);
    }
    let attr_name = format!("on{kind}");
    let mut prevented = false;
    for n in path {
        let this = element(it, n)?;
        it.put(host, &event, "currentTarget", this.clone())?;
        // A handler set as a property (`el.onclick = f`) or written in the attribute.
        let prop = match &this {
            Value::Obj(o) => it.get_own(*o, &format!("__{attr_name}")),
            _ => None,
        };
        let inline = match prop {
            Some(f) if it.is_callable(&f) => Some(f),
            _ => match host.attr(n, &attr_name) {
                Some(code) => match it.compile_handler(&code) {
                    Ok(f) => Some(f),
                    Err(e) => {
                        host.log(&format!("SyntaxError in {attr_name}: {e}"));
                        None
                    }
                },
                None => None,
            },
        };
        if let Some(f) = inline {
            let r = it.call(host, &f, this.clone(), std::slice::from_ref(&event))?;
            if matches!(r, Value::Bool(false)) {
                prevented = true;
            }
        }
        let listeners: Vec<Value> = it
            .listeners
            .iter()
            .filter(|(t, k, _)| *t == Target::Node(n) && &**k == kind)
            .map(|(_, _, f)| f.clone())
            .collect();
        for f in listeners {
            call_listener(it, host, &f, this.clone(), &event)?;
        }
        let stop = it.get_value(host, &event, "__stop")?;
        if it.truthy(&stop) {
            break;
        }
    }
    let stop = it.get_value(host, &event, "__stop")?;
    if !it.truthy(&stop) {
        let listeners: Vec<Value> = it
            .listeners
            .iter()
            .filter(|(t, k, _)| *t == Target::Window && &**k == kind)
            .map(|(_, _, f)| f.clone())
            .collect();
        let window = Value::Obj(it.global);
        for f in listeners {
            call_listener(it, host, &f, window.clone(), &event)?;
        }
    }
    let flag = it.get_value(host, &event, "defaultPrevented")?;
    Ok(prevented || it.truthy(&flag))
}

pub fn dispatch_window(it: &mut Interp, host: &mut dyn Host, kind: &str) -> Result<(), Ctrl> {
    let window = Value::Obj(it.global);
    let event = event_object(it, host, kind, window.clone())?;
    let listeners: Vec<Value> = it
        .listeners
        .iter()
        .filter(|(t, k, _)| *t == Target::Window && &**k == kind)
        .map(|(_, _, f)| f.clone())
        .collect();
    for f in listeners {
        call_listener(it, host, &f, window.clone(), &event)?;
    }
    // `window.onload = …`
    if let Some(f) = it.get_own(it.global, &format!("on{kind}")) {
        if it.is_callable(&f) {
            it.call(host, &f, window, std::slice::from_ref(&event))?;
        }
    }
    Ok(())
}
