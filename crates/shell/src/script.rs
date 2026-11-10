//! A page's scripts: which `<script>` elements run, the DOM as the interpreter sees it
//! (`js::Host`), and running script work on a thread with the stack it needs.
//!
//! Scripts run once the document is parsed and its form controls prepared, before styles,
//! so what they change is styled like the rest. They can change the DOM later too (events,
//! timers); then the page is styled and laid out again. Stylesheets of a page with scripts
//! are parsed without dropping rules for names the document lacks, since scripts can add
//! them (see CLAUDE.md, CSS invariants).
//!
//! Scripts cannot read cookies, fetch, or navigate, and nothing they store reaches disk.

use std::time::Instant;

use html::{Dom, NodeId, NodeKind};

use crate::forms::Forms;

/// Scripts per page, and bytes of script source run per page.
const MAX_SCRIPTS: usize = 64;
const MAX_SCRIPT_BYTES: usize = 4 << 20;

/// The scripts of a live page.
pub struct PageScript {
    pub runtime: js::Runtime,
    started: Instant,
    epoch_ms: f64,
}

impl PageScript {
    fn new() -> PageScript {
        let epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64() * 1000.0);
        PageScript {
            runtime: js::Runtime::new(),
            started: Instant::now(),
            epoch_ms,
        }
    }

    fn tick(&mut self) {
        let now = self.started.elapsed().as_secs_f64() * 1000.0;
        self.runtime.set_time(now, self.epoch_ms);
    }

    /// How long until the next timer is due, if any.
    pub fn next_timer(&self) -> Option<std::time::Duration> {
        let due = self.runtime.next_timer()?;
        let now = self.started.elapsed().as_secs_f64() * 1000.0;
        Some(std::time::Duration::from_secs_f64(
            ((due - now).max(0.0)) / 1000.0,
        ))
    }
}

/// Run `f` on a thread with the stack scripts need, waiting for it.
pub fn on_script_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("script".into())
            .stack_size(js::STACK_SIZE)
            .spawn_scoped(s, f)
            .expect("spawn a script thread")
            .join()
            .unwrap_or_else(|e| std::panic::resume_unwind(e))
    })
}

/// The DOM and form state, as scripts see them.
pub struct DomHost<'a> {
    pub dom: &'a mut Dom,
    pub forms: &'a mut Forms,
    pub url: String,
    /// Something scripts did changes what the page shows.
    pub changed: bool,
}

impl DomHost<'_> {
    fn control(&self, node: u32) -> Option<usize> {
        self.forms
            .controls
            .binary_search_by_key(&node, |c| c.node)
            .ok()
    }

    /// Copy `node` of `from` (a parsed fragment) and its subtree into this document.
    fn graft(&mut self, from: &Dom, node: NodeId) -> Option<NodeId> {
        let copy = match from.kind(node) {
            NodeKind::Text(t) => self.dom.create_text(t),
            NodeKind::Element(e) => {
                let tag = from.atom_name(e.tag).to_string();
                let c = self.dom.create_element(&tag);
                for (name, value) in e.attrs() {
                    let name = from.atom_name(name).to_string();
                    self.dom.set_attr(c, &name, value);
                }
                for child in from.children(node) {
                    if let Some(g) = self.graft(from, child) {
                        self.dom.append_child(c, g);
                    }
                }
                c
            }
            _ => return None,
        };
        Some(copy)
    }

    fn clear(&mut self, node: u32) {
        let children: Vec<NodeId> = self.dom.children(node).collect();
        for c in children {
            self.dom.detach(c);
        }
    }
}

impl js::Host for DomHost<'_> {
    fn select(&mut self, root: Option<u32>, selector: &str) -> Vec<u32> {
        css::select(self.dom, root.unwrap_or(Dom::ROOT), selector)
    }

    fn matches(&mut self, node: u32, selector: &str) -> bool {
        css::matches_selector(self.dom, node, selector)
    }

    fn tag(&self, node: u32) -> Option<String> {
        self.dom.tag_name(node).map(str::to_ascii_lowercase)
    }

    fn is_text(&self, node: u32) -> bool {
        matches!(self.dom.kind(node), NodeKind::Text(_))
    }

    fn text(&self, node: u32) -> String {
        match self.dom.kind(node) {
            NodeKind::Text(t) => t.to_string(),
            _ => self.dom.text_content(node),
        }
    }

    fn set_text(&mut self, node: u32, text: &str) {
        self.changed = true;
        if self.is_text(node) {
            self.dom.set_text(node, text);
            return;
        }
        self.clear(node);
        if !text.is_empty() {
            let t = self.dom.create_text(text);
            self.dom.append_child(node, t);
        }
    }

    fn set_html(&mut self, node: u32, source: &str) {
        self.changed = true;
        self.clear(node);
        let fragment = html::parse(source);
        // The tree builder may or may not imply <html><body> around a fragment.
        let body = fragment.find_first("body").unwrap_or(Dom::ROOT);
        let children: Vec<NodeId> = fragment
            .children(body)
            .filter(|&c| !matches!(fragment.tag_name(c), Some("html" | "head")))
            .collect();
        for child in children {
            if let Some(g) = self.graft(&fragment, child) {
                self.dom.append_child(node, g);
            }
        }
    }

    fn attr(&self, node: u32, name: &str) -> Option<String> {
        self.dom.attr(node, name).map(str::to_string)
    }

    fn set_attr(&mut self, node: u32, name: &str, value: &str) {
        self.changed = true;
        self.dom.set_attr(node, name, value);
    }

    fn remove_attr(&mut self, node: u32, name: &str) {
        self.changed = true;
        self.dom.remove_attr(node, name);
    }

    fn parent(&self, node: u32) -> Option<u32> {
        self.dom.parent(node)
    }

    fn children(&self, node: u32) -> Vec<u32> {
        self.dom.children(node).collect()
    }

    fn create_element(&mut self, tag: &str) -> u32 {
        self.dom.create_element(tag)
    }

    fn create_text(&mut self, text: &str) -> u32 {
        self.dom.create_text(text)
    }

    fn append(&mut self, parent: u32, child: u32) -> bool {
        self.changed = true;
        self.dom.append_child(parent, child)
    }

    fn insert_before(&mut self, parent: u32, child: u32, before: u32) -> bool {
        self.changed = true;
        self.dom.insert_before(parent, child, before)
    }

    fn remove(&mut self, node: u32) {
        self.changed = true;
        self.dom.detach(node);
    }

    fn value(&self, node: u32) -> Option<String> {
        self.control(node)
            .map(|i| self.forms.controls[i].value.clone())
    }

    fn set_value(&mut self, node: u32, value: &str) {
        if let Some(i) = self.control(node) {
            self.changed = true;
            self.forms.set_value(self.dom, i, value);
        }
    }

    fn checked(&self, node: u32) -> Option<bool> {
        self.control(node).map(|i| self.forms.controls[i].checked)
    }

    fn set_checked(&mut self, node: u32, checked: bool) {
        if let Some(i) = self.control(node) {
            if self.forms.controls[i].checked != checked {
                self.changed = true;
                self.forms.toggle(self.dom, i);
            }
        }
    }

    fn url(&self) -> String {
        self.url.clone()
    }

    fn log(&mut self, line: &str) {
        eprintln!("console: {line}");
    }
}

/// Where a page's external scripts come from.
pub enum ScriptFrom<'a> {
    Network(Option<(&'a net::Cookies, &'a net::Context)>),
    /// A sleeping page waking: the scripts as stored with it.
    Stored(&'a [(String, net::Wire)]),
    /// Inline scripts only.
    Inline,
}

/// Whether a `<script>` is classic JavaScript Broxee runs (not a module, JSON or template).
fn runs(dom: &Dom, node: NodeId) -> bool {
    match dom
        .attr(node, "type")
        .map(|t| t.trim().to_ascii_lowercase())
    {
        None => true,
        Some(t) => matches!(
            t.as_str(),
            "" | "text/javascript"
                | "application/javascript"
                | "text/ecmascript"
                | "application/ecmascript"
        ),
    }
}

/// Run the page's scripts in document order, then `DOMContentLoaded` and `load`. Returns the
/// live scripts (`None` when the page has none) and the external scripts as they arrived,
/// for a sleeping tab.
pub fn run_page_scripts(
    dom: &mut Dom,
    forms: &mut Forms,
    url: &net::Url,
    from: ScriptFrom,
) -> (Option<Box<PageScript>>, Vec<(String, net::Wire)>) {
    let nodes: Vec<NodeId> = dom
        .descendants(Dom::ROOT)
        .filter(|&n| dom.tag_name(n) == Some("script") && runs(dom, n))
        .take(MAX_SCRIPTS)
        .collect();
    if nodes.is_empty() {
        return (None, Vec::new());
    }
    enum Source {
        Inline(String),
        External(net::Url),
    }
    let sources: Vec<Source> = nodes
        .iter()
        .filter_map(|&n| match dom.attr(n, "src") {
            Some(src) => url.join(src.trim()).ok().map(Source::External),
            None => Some(Source::Inline(dom.text_content(n))),
        })
        .collect();
    let mut wires = Vec::new();
    let mut texts: Vec<(String, String)> = Vec::new();
    let mut total = 0;
    for source in sources {
        let (name, text) = match source {
            Source::Inline(text) => ("inline script".to_string(), Some(text)),
            Source::External(u) => {
                let key = u.to_string();
                let bytes = match &from {
                    ScriptFrom::Network(jar) => match net::fetch_in(&u, *jar) {
                        Ok(mut resp) if resp.status == 200 => {
                            let wire = if resp.wire.bytes.is_empty() {
                                net::Wire {
                                    encoding: net::Encoding::Identity,
                                    bytes: resp.body.clone(),
                                }
                            } else {
                                std::mem::take(&mut resp.wire)
                            };
                            wires.push((key.clone(), wire));
                            Some(resp.body)
                        }
                        _ => None,
                    },
                    ScriptFrom::Stored(stored) => {
                        stored.iter().find(|(k, _)| *k == key).and_then(|(k, w)| {
                            wires.push((k.clone(), w.clone()));
                            w.decode(net::MAX_BODY).ok()
                        })
                    }
                    ScriptFrom::Inline => None,
                };
                (key, bytes.map(|b| String::from_utf8_lossy(&b).into_owned()))
            }
        };
        let Some(text) = text else { continue };
        total += text.len();
        if total > MAX_SCRIPT_BYTES {
            break;
        }
        texts.push((name, text));
    }
    let mut script = Box::new(PageScript::new());
    let page_url = url.to_string();
    on_script_stack(|| {
        let mut host = DomHost {
            dom,
            forms,
            url: page_url,
            changed: false,
        };
        script.tick();
        for (name, text) in &texts {
            script.runtime.run(&mut host, text, name);
        }
        script.runtime.loaded(&mut host);
    });
    (Some(script), wires)
}

/// Fire `kind` at `node` (a click). Returns (the page changed, a listener prevented the
/// default action).
pub fn dispatch(
    script: &mut PageScript,
    dom: &mut Dom,
    forms: &mut Forms,
    url: &net::Url,
    node: NodeId,
    kind: &str,
) -> (bool, bool) {
    {
        let host = DomHost {
            dom,
            forms,
            url: String::new(),
            changed: false,
        };
        if !script.runtime.listens(&host, node, kind) {
            return (false, false);
        }
    }
    let page_url = url.to_string();
    on_script_stack(|| {
        let mut host = DomHost {
            dom,
            forms,
            url: page_url,
            changed: false,
        };
        script.tick();
        let prevented = script.runtime.dispatch(&mut host, node, kind);
        (host.changed, prevented)
    })
}

/// Run the timers that are due. Returns whether the page changed.
pub fn run_timers(
    script: &mut PageScript,
    dom: &mut Dom,
    forms: &mut Forms,
    url: &net::Url,
) -> bool {
    let page_url = url.to_string();
    on_script_stack(|| {
        let mut host = DomHost {
            dom,
            forms,
            url: page_url,
            changed: false,
        };
        script.tick();
        script.runtime.run_timers(&mut host);
        host.changed
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(src: &str) -> (Dom, Forms, Option<Box<PageScript>>) {
        let mut dom = html::parse(src);
        let mut forms = Forms::prepare(&mut dom);
        let url = net::Url::parse("https://example.org/").unwrap();
        let (script, _) = run_page_scripts(&mut dom, &mut forms, &url, ScriptFrom::Inline);
        (dom, forms, script)
    }

    #[test]
    fn scripts_change_the_document_before_it_is_styled() {
        let (dom, _, script) = page(
            "<p id=a>old</p><ul></ul><script>\
             document.querySelector('#a').textContent = 'new';\
             const ul = document.querySelector('ul');\
             for (const t of ['x', 'y']) { const li = document.createElement('li'); li.textContent = t; ul.appendChild(li) }\
             document.getElementById('a').classList.add('on', 'big');\
             document.querySelector('p').style.color = 'red';\
             document.querySelector('ul').insertAdjacentHTML('beforeend', '<li>z</li>');\
             </script><div id=h></div><script>document.getElementById('h').innerHTML = '<b class=k>bold</b> text'</script>",
        );
        assert!(script.is_some());
        let p = dom.find_first("p").unwrap();
        assert_eq!(dom.text_content(p), "new");
        assert_eq!(dom.attr(p, "class"), Some("on big"));
        assert_eq!(dom.attr(p, "style"), Some("color: red"));
        assert_eq!(dom.text_content(dom.find_first("ul").unwrap()), "xyz");
        let b = css::select(&dom, Dom::ROOT, "#h > b.k");
        assert_eq!(b.len(), 1, "innerHTML is parsed");
        assert_eq!(dom.text_content(b[0]), "bold");
    }

    #[test]
    fn clicks_and_timers_run_listeners() {
        let (mut dom, mut forms, script) = page(
            "<button id=b>0</button><a href=/x id=l>link</a><input id=i value=v>\
             <script>\
             let n = 0; const b = document.getElementById('b');\
             b.addEventListener('click', e => { n++; b.textContent = n; document.getElementById('i').value = 'clicked' });\
             document.getElementById('l').onclick = e => e.preventDefault();\
             setTimeout(() => { b.textContent = 'timer' }, 0);\
             </script>",
        );
        let mut script = script.unwrap();
        let url = net::Url::parse("https://example.org/").unwrap();
        let b = dom.find_first("button").unwrap();
        let text = dom.children(b).next().unwrap();
        // A click on the button's text bubbles to the button.
        assert_eq!(
            dispatch(&mut script, &mut dom, &mut forms, &url, text, "click"),
            (true, false)
        );
        assert_eq!(dom.text_content(b), "1");
        let input = forms
            .controls
            .iter()
            .find(|c| c.kind == crate::forms::Kind::Text)
            .unwrap();
        assert_eq!(input.value, "clicked", "value goes through the form state");
        let link = dom.find_first("a").unwrap();
        assert_eq!(
            dispatch(&mut script, &mut dom, &mut forms, &url, link, "click"),
            (false, true)
        );
        let p = dom.find_first("input").unwrap();
        assert!(!dispatch(&mut script, &mut dom, &mut forms, &url, p, "click").1);
        assert!(script.next_timer().is_some());
        assert!(run_timers(&mut script, &mut dom, &mut forms, &url));
        assert_eq!(dom.text_content(b), "timer");
        assert!(script.next_timer().is_none());
    }

    #[test]
    fn pages_without_scripts_have_none() {
        let (_, _, script) = page("<p>x</p><script type=application/json>{}</script>");
        assert!(script.is_none());
    }
}
