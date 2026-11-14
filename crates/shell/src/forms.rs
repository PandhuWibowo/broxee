//! Form controls: what each `<input>`, `<textarea>`, `<select>` and `<button>` shows, its
//! state while the page is open, and what a form sends.
//!
//! Controls are drawn with the page's own layout and paint. `prepare` gives each control
//! child nodes for what it shows (`broxee-placeholder`, a value text node, `broxee-check`,
//! `broxee-arrow`), styled by `ua.css`, and appends them after the page's own nodes so no
//! node id changes. Editing changes those text nodes and the page is laid out again.
//!
//! State lives with the page and dies with it. A sleeping tab rebuilds its page from the
//! sources, so typed values are not kept, and passwords are never written anywhere.

use html::{Dom, NodeId, NodeKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Password,
    TextArea,
    Checkbox,
    Radio,
    Submit,
    Reset,
    Button,
    Select,
    Hidden,
}

pub struct Control {
    pub node: NodeId,
    pub kind: Kind,
    /// The text node showing the value (fields, buttons, the chosen option).
    shown: Option<NodeId>,
    /// The text node inside `broxee-placeholder`.
    placeholder_text: Option<NodeId>,
    placeholder: String,
    /// The `broxee-check` / `broxee-uncheck` element of a checkbox or radio button.
    mark: Option<NodeId>,
    pub value: String,
    pub checked: bool,
    /// `<option>` elements of a select, and which one is chosen.
    options: Vec<NodeId>,
    pub selected: usize,
}

#[derive(Default)]
pub struct Forms {
    /// In document order.
    pub controls: Vec<Control>,
}

/// A line of a select's dropdown: an `<optgroup>` label, or an option to choose.
#[derive(Clone, Debug, PartialEq)]
pub enum OptionRow {
    Group(String),
    Choice {
        label: String,
        /// Its index among the select's options.
        index: usize,
        disabled: bool,
    },
}

/// What submitting a form asks for: a GET of `url`, or a POST of `body` to it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
#[derive(Debug, PartialEq)]
pub struct Submission {
    pub url: net::Url,
    pub body: Option<net::Body>,
}

fn kind_of(dom: &Dom, node: NodeId) -> Option<Kind> {
    let tag = dom.tag_name(node)?;
    let ty = || {
        dom.attr(node, "type")
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    };
    Some(match tag {
        "input" => match ty().as_str() {
            "password" => Kind::Password,
            "checkbox" => Kind::Checkbox,
            "radio" => Kind::Radio,
            "submit" | "image" => Kind::Submit,
            "reset" => Kind::Reset,
            "button" | "file" | "color" | "range" => Kind::Button,
            "hidden" => Kind::Hidden,
            _ => Kind::Text,
        },
        "textarea" => Kind::TextArea,
        "select" => Kind::Select,
        "button" => match ty().as_str() {
            "reset" => Kind::Reset,
            "button" => Kind::Button,
            _ => Kind::Submit,
        },
        _ => return None,
    })
}

/// Collapse whitespace runs to single spaces and trim, as an option's label shows.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Control {
    fn shows(&self) -> String {
        match self.kind {
            Kind::Password => "•".repeat(self.value.chars().count()),
            _ => self.value.clone(),
        }
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
impl Forms {
    /// Find the controls of `dom` and give them the nodes they draw with.
    pub fn prepare(dom: &mut Dom) -> Forms {
        let found: Vec<(NodeId, Kind)> = dom
            .descendants(Dom::ROOT)
            .filter_map(|n| Some((n, kind_of(dom, n)?)))
            .collect();
        let mut controls = Vec::with_capacity(found.len());
        for (node, kind) in found {
            let attr = |name: &str| dom.attr(node, name).map(str::to_string);
            let mut c = Control {
                node,
                kind,
                shown: None,
                placeholder_text: None,
                placeholder: attr("placeholder").unwrap_or_default(),
                mark: None,
                value: attr("value").unwrap_or_default(),
                checked: dom.attr(node, "checked").is_some(),
                options: Vec::new(),
                selected: 0,
            };
            let is_button_element = dom.tag_name(node) == Some("button");
            match kind {
                Kind::Text | Kind::Password => {
                    let p = dom.append_element(node, "broxee-placeholder");
                    c.placeholder_text = Some(dom.append_text(p, ""));
                    c.shown = Some(dom.append_text(node, ""));
                    c.value = c.value.replace(['\r', '\n'], "");
                }
                Kind::TextArea => {
                    let texts: Vec<NodeId> = dom
                        .children(node)
                        .filter(|&t| matches!(dom.kind(t), NodeKind::Text(_)))
                        .collect();
                    let mut value = dom.text_content(node);
                    // A newline right after <textarea> is not part of the value.
                    if let Some(rest) = value.strip_prefix('\n') {
                        value = rest.to_string();
                    }
                    for &t in &texts {
                        dom.set_text(t, "");
                    }
                    c.value = value;
                    let p = dom.append_element(node, "broxee-placeholder");
                    c.placeholder_text = Some(dom.append_text(p, ""));
                    c.shown = Some(match texts.first() {
                        Some(&t) => t,
                        None => dom.append_text(node, ""),
                    });
                }
                Kind::Checkbox | Kind::Radio => {
                    c.mark = Some(dom.append_element(node, "broxee-uncheck"));
                    if c.value.is_empty() && dom.attr(node, "value").is_none() {
                        c.value = "on".into();
                    }
                }
                Kind::Submit | Kind::Reset | Kind::Button if !is_button_element => {
                    let label = match (kind, dom.attr(node, "value")) {
                        (_, Some(v)) => v.to_string(),
                        (Kind::Submit, None) => "Submit".into(),
                        (Kind::Reset, None) => "Reset".into(),
                        _ if dom.attr(node, "type") == Some("file") => "Choose File".into(),
                        _ => String::new(),
                    };
                    dom.append_text(node, &label);
                }
                Kind::Select => {
                    let options: Vec<NodeId> = dom
                        .descendants(node)
                        .filter(|&o| dom.tag_name(o) == Some("option"))
                        .collect();
                    c.selected = options
                        .iter()
                        .position(|&o| dom.attr(o, "selected").is_some())
                        .unwrap_or(0);
                    c.options = options;
                    let v = dom.append_element(node, "broxee-value");
                    c.shown = Some(dom.append_text(v, ""));
                    let arrow = dom.append_element(node, "broxee-arrow");
                    dom.append_text(arrow, "▼");
                }
                _ => {}
            }
            controls.push(c);
        }
        // One checked radio button per group: the last one wins, as in browsers.
        for i in 0..controls.len() {
            if controls[i].kind == Kind::Radio && controls[i].checked {
                for j in 0..i {
                    if controls[j].checked && same_group(dom, &controls[i], &controls[j]) {
                        controls[j].checked = false;
                    }
                }
            }
        }
        let mut forms = Forms { controls };
        for i in 0..forms.controls.len() {
            forms.show(dom, i);
        }
        forms
    }

    /// Bring the nodes of control `i` in line with its state.
    fn show(&mut self, dom: &mut Dom, i: usize) {
        let c = &self.controls[i];
        match c.kind {
            Kind::Select => {
                let label = c.options.get(c.selected).map(|&o| {
                    dom.attr(o, "label")
                        .map(collapse)
                        .unwrap_or_else(|| collapse(&dom.text_content(o)))
                });
                if let Some(t) = c.shown {
                    dom.set_text(t, &label.unwrap_or_default());
                }
            }
            Kind::Checkbox | Kind::Radio => {
                if let Some(m) = c.mark {
                    dom.set_tag(
                        m,
                        if c.checked {
                            "broxee-check"
                        } else {
                            "broxee-uncheck"
                        },
                    );
                }
            }
            _ => {
                let shows = c.shows();
                let empty = shows.is_empty();
                if let Some(t) = c.shown {
                    dom.set_text(t, &shows);
                }
                if let Some(p) = c.placeholder_text {
                    dom.set_text(
                        p,
                        if empty {
                            &self.controls[i].placeholder
                        } else {
                            ""
                        },
                    );
                }
            }
        }
    }

    /// The control that `node` is, or is inside.
    pub fn at(&self, dom: &Dom, node: NodeId) -> Option<usize> {
        let mut n = Some(node);
        while let Some(id) = n {
            if let Ok(i) = self.controls.binary_search_by_key(&id, |c| c.node) {
                return Some(i);
            }
            n = dom.parent(id);
        }
        None
    }

    /// The text nodes of field `i` that show its value and its placeholder.
    pub fn text_nodes(&self, i: usize) -> (Option<NodeId>, Option<NodeId>) {
        (self.controls[i].shown, self.controls[i].placeholder_text)
    }

    pub fn is_text(&self, i: usize) -> bool {
        matches!(
            self.controls[i].kind,
            Kind::Text | Kind::Password | Kind::TextArea
        )
    }

    /// Whether Tab stops at control `i`.
    pub fn focusable(&self, dom: &Dom, i: usize) -> bool {
        let c = &self.controls[i];
        c.kind != Kind::Hidden && dom.attr(c.node, "disabled").is_none()
    }

    pub fn set_value(&mut self, dom: &mut Dom, i: usize, value: &str) {
        if self.controls[i].kind == Kind::Select {
            // A select takes the option with that value.
            let options = self.controls[i].options.clone();
            let found = options.iter().position(|&o| {
                dom.attr(o, "value")
                    .map(str::to_string)
                    .unwrap_or_else(|| collapse(&dom.text_content(o)))
                    == value
            });
            if let Some(k) = found {
                self.controls[i].selected = k;
                self.show(dom, i);
            }
            return;
        }
        let c = &mut self.controls[i];
        c.value = if c.kind == Kind::TextArea {
            value.to_string()
        } else {
            value.replace(['\r', '\n'], "")
        };
        self.show(dom, i);
    }

    /// A click or Space on a checkbox or radio button. Returns whether anything changed.
    pub fn toggle(&mut self, dom: &mut Dom, i: usize) -> bool {
        match self.controls[i].kind {
            Kind::Checkbox => {
                self.controls[i].checked = !self.controls[i].checked;
                self.show(dom, i);
                true
            }
            Kind::Radio if !self.controls[i].checked => {
                for j in 0..self.controls.len() {
                    if j != i
                        && self.controls[j].checked
                        && same_group(dom, &self.controls[i], &self.controls[j])
                    {
                        self.controls[j].checked = false;
                        self.show(dom, j);
                    }
                }
                self.controls[i].checked = true;
                self.show(dom, i);
                true
            }
            _ => false,
        }
    }

    /// What select `i`'s dropdown lists: its options in order, under their group labels.
    pub fn option_rows(&self, dom: &Dom, i: usize) -> Vec<OptionRow> {
        let mut rows = Vec::new();
        let mut group: Option<NodeId> = None;
        for (index, &o) in self.controls[i].options.iter().enumerate() {
            let parent = dom
                .parent(o)
                .filter(|&p| dom.tag_name(p) == Some("optgroup"));
            if parent != group {
                group = parent;
                if let Some(g) = parent {
                    rows.push(OptionRow::Group(collapse(
                        dom.attr(g, "label").unwrap_or(""),
                    )));
                }
            }
            let label = dom
                .attr(o, "label")
                .map(collapse)
                .unwrap_or_else(|| collapse(&dom.text_content(o)));
            let disabled = dom.attr(o, "disabled").is_some()
                || parent.is_some_and(|g| dom.attr(g, "disabled").is_some());
            rows.push(OptionRow::Choice {
                label,
                index,
                disabled,
            });
        }
        rows
    }

    /// What control `i` holds, as scripts read it and a form sends it: a select's chosen
    /// option's `value` (or its text), anything else its typed or set value.
    pub fn value_of(&self, dom: &Dom, i: usize) -> String {
        let c = &self.controls[i];
        match (c.kind, c.options.get(c.selected)) {
            (Kind::Select, Some(&o)) => dom
                .attr(o, "value")
                .map(str::to_string)
                .unwrap_or_else(|| collapse(&dom.text_content(o))),
            (Kind::Select, None) => String::new(),
            _ => c.value.clone(),
        }
    }

    /// Choose option `k` of select `i`. Returns whether the choice changed.
    pub fn choose_index(&mut self, dom: &mut Dom, i: usize, k: usize) -> bool {
        let c = &mut self.controls[i];
        if c.kind != Kind::Select || k >= c.options.len() || k == c.selected {
            return false;
        }
        c.selected = k;
        self.show(dom, i);
        true
    }

    /// Move a select's choice by `delta`, wrapping. Returns whether it changed.
    pub fn choose(&mut self, dom: &mut Dom, i: usize, delta: isize) -> bool {
        let c = &mut self.controls[i];
        let n = c.options.len() as isize;
        if c.kind != Kind::Select || n < 2 {
            return false;
        }
        c.selected = (c.selected as isize + delta).rem_euclid(n) as usize;
        self.show(dom, i);
        true
    }

    /// Put every control of the form that `i` belongs to back to how the page set it.
    pub fn reset(&mut self, dom: &mut Dom, i: usize) {
        let form = form_of(dom, self.controls[i].node);
        let fresh_values: Vec<(usize, String, bool, usize)> = self
            .controls
            .iter()
            .enumerate()
            .filter(|(_, c)| form_of(dom, c.node) == form)
            .map(|(j, c)| {
                let value = match c.kind {
                    Kind::TextArea => {
                        // The page's own text nodes were emptied; the original value is gone,
                        // so a reset textarea is empty, like one the page left empty.
                        String::new()
                    }
                    Kind::Checkbox | Kind::Radio => c.value.clone(),
                    _ => dom.attr(c.node, "value").unwrap_or("").to_string(),
                };
                let selected = c
                    .options
                    .iter()
                    .position(|&o| dom.attr(o, "selected").is_some())
                    .unwrap_or(0);
                (j, value, dom.attr(c.node, "checked").is_some(), selected)
            })
            .collect();
        for (j, value, checked, selected) in fresh_values {
            let c = &mut self.controls[j];
            if matches!(c.kind, Kind::Text | Kind::Password | Kind::TextArea) {
                c.value = value;
            }
            c.checked = checked;
            c.selected = selected;
            self.show(dom, j);
        }
    }

    /// What submitting the form of control `i` sends, `i` being the button pressed (or the
    /// field where Enter was pressed). `None` when `i` is in no form.
    /// The first control of `i`'s form that stops it from being sent, and why (constraint
    /// validation: `required`, `type=email`, `url`, `number` with `min` / `max`). `None`
    /// when the form or the pressed button says `novalidate`.
    pub fn invalid(&self, dom: &Dom, i: usize) -> Option<(usize, &'static str)> {
        let form = form_of(dom, self.controls[i].node)?;
        if dom.attr(form, "novalidate").is_some()
            || dom.attr(self.controls[i].node, "formnovalidate").is_some()
        {
            return None;
        }
        for (j, c) in self.controls.iter().enumerate() {
            if form_of(dom, c.node) != Some(form) || dom.attr(c.node, "disabled").is_some() {
                continue;
            }
            let required = dom.attr(c.node, "required").is_some();
            let ty = input_type(dom, c.node);
            let value = c.value.trim();
            let message = match c.kind {
                Kind::Text | Kind::Password | Kind::TextArea if required && c.value.is_empty() => {
                    Some("Please fill in this field.")
                }
                Kind::Checkbox if required && !c.checked => Some("Please check this box."),
                Kind::Radio if required && !self.group_checked(dom, j) => {
                    Some("Please choose one of these options.")
                }
                Kind::Select if required && self.value_of(dom, j).is_empty() => {
                    Some("Please choose an option.")
                }
                Kind::Text if !value.is_empty() && ty == "email" && !is_email(value) => {
                    Some("Please enter an email address.")
                }
                Kind::Text
                    if !value.is_empty() && ty == "url" && net::Url::parse(value).is_err() =>
                {
                    Some("Please enter a URL.")
                }
                Kind::Text if !value.is_empty() && ty == "number" => match value.parse::<f64>() {
                    Err(_) => Some("Please enter a number."),
                    Ok(n) => {
                        let bound = |name| {
                            dom.attr(c.node, name)
                                .and_then(|v| v.trim().parse::<f64>().ok())
                        };
                        let low = bound("min").is_some_and(|m| n < m);
                        let high = bound("max").is_some_and(|m| n > m);
                        (low || high).then_some("Please enter a number within the allowed range.")
                    }
                },
                _ => None,
            };
            if let Some(m) = message {
                return Some((j, m));
            }
        }
        None
    }

    /// Whether radio button `i`'s group has one checked.
    fn group_checked(&self, dom: &Dom, i: usize) -> bool {
        let me = &self.controls[i];
        me.checked
            || self
                .controls
                .iter()
                .any(|c| c.checked && same_group(dom, me, c))
    }

    /// One step of a number field (↑ ↓): its value moved by `step` (1 unless set), kept
    /// within `min` and `max`.
    pub fn stepped(&self, dom: &Dom, i: usize, up: bool) -> Option<String> {
        let c = &self.controls[i];
        if c.kind != Kind::Text || input_type(dom, c.node) != "number" {
            return None;
        }
        let attr = |name| {
            dom.attr(c.node, name)
                .and_then(|v| v.trim().parse::<f64>().ok())
        };
        let step = attr("step").filter(|s| *s > 0.0).unwrap_or(1.0);
        let now = c.value.trim().parse::<f64>().unwrap_or(0.0);
        let mut next = if up { now + step } else { now - step };
        if let Some(m) = attr("min") {
            next = next.max(m);
        }
        if let Some(m) = attr("max") {
            next = next.min(m);
        }
        // Whole numbers without ".0"; steps like 0.1 without float noise.
        let rounded = (next * 1e9).round() / 1e9;
        Some(if rounded.fract() == 0.0 {
            format!("{}", rounded as i64)
        } else {
            format!("{rounded}")
        })
    }

    pub fn submission(&self, dom: &Dom, i: usize, document: &net::Url) -> Option<Submission> {
        let form = form_of(dom, self.controls[i].node)?;
        let submitter = matches!(self.controls[i].kind, Kind::Submit)
            .then_some(i)
            .or_else(|| {
                // Enter in a field: the form's first submit button counts as pressed.
                self.controls
                    .iter()
                    .position(|c| c.kind == Kind::Submit && form_of(dom, c.node) == Some(form))
            });
        let mut pairs: Vec<(String, String)> = Vec::new();
        let mut files: Vec<String> = Vec::new();
        for (j, c) in self.controls.iter().enumerate() {
            if form_of(dom, c.node) != Some(form) || dom.attr(c.node, "disabled").is_some() {
                continue;
            }
            let Some(name) = dom.attr(c.node, "name").filter(|n| !n.is_empty()) else {
                continue;
            };
            let value = match c.kind {
                Kind::Text | Kind::Password | Kind::TextArea | Kind::Hidden => {
                    // Line breaks are sent as CR LF.
                    c.value.replace("\r\n", "\n").replace('\n', "\r\n")
                }
                Kind::Checkbox | Kind::Radio if c.checked => c.value.clone(),
                Kind::Select if c.options.get(c.selected).is_some() => self.value_of(dom, j),
                Kind::Select => continue,
                Kind::Submit if Some(j) == submitter => {
                    dom.attr(c.node, "value").unwrap_or("").to_string()
                }
                // A file input sends an empty file part (multipart) or nothing.
                Kind::Button if input_type(dom, c.node) == "file" => {
                    files.push(name.to_string());
                    continue;
                }
                _ => continue,
            };
            pairs.push((name.to_string(), value));
        }
        let encoded = urlencode(&pairs);
        let button_attr =
            |name: &str| submitter.and_then(|s| dom.attr(self.controls[s].node, name));
        let action = button_attr("formaction")
            .or_else(|| dom.attr(form, "action"))
            .map(str::trim)
            .filter(|a| !a.is_empty());
        let mut url = match action {
            Some(a) => document.join(a).ok()?,
            None => document.clone(),
        };
        let method = button_attr("formmethod")
            .or_else(|| dom.attr(form, "method"))
            .unwrap_or("get")
            .trim()
            .to_ascii_lowercase();
        // Drop the fragment, then the query.
        let path = url.path.split('#').next().unwrap_or("").to_string();
        let path = path.split('?').next().unwrap_or("").to_string();
        let enctype = button_attr("formenctype")
            .or_else(|| dom.attr(form, "enctype"))
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if method == "post" {
            url.path = url.path.split('#').next().unwrap_or("").to_string();
            let body = if enctype == "multipart/form-data" {
                multipart(&pairs, &files)
            } else {
                net::Body {
                    content_type: "application/x-www-form-urlencoded".into(),
                    bytes: encoded.into_bytes(),
                }
            };
            Some(Submission {
                url,
                body: Some(body),
            })
        } else {
            url.path = format!("{path}?{encoded}");
            Some(Submission { url, body: None })
        }
    }
}

/// An input's `type`, lowercase.
fn input_type(dom: &Dom, node: NodeId) -> String {
    dom.attr(node, "type")
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// A plausible address: something@domain, no spaces (what browsers check for type=email).
fn is_email(v: &str) -> bool {
    match v.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && !domain.is_empty()
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !v.contains(char::is_whitespace)
                && !domain.contains('@')
        }
        None => false,
    }
}

/// A `multipart/form-data` body: each field a part, each file input an empty file part
/// (Broxee cannot pick files yet), names quoted as the HTML spec says.
fn multipart(pairs: &[(String, String)], files: &[String]) -> net::Body {
    let quote = |n: &str| {
        n.replace('"', "%22")
            .replace('\r', "%0D")
            .replace('\n', "%0A")
    };
    // A boundary that cannot occur in the values.
    let mut seed: u64 = 0xcbf2_9ce4_8422_2325;
    for (k, v) in pairs {
        for b in k.bytes().chain(v.bytes()) {
            seed = (seed ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    let mut boundary = format!("----BroxeeFormBoundary{seed:016x}");
    while pairs
        .iter()
        .any(|(k, v)| k.contains(&boundary) || v.contains(&boundary))
    {
        boundary.push('x');
    }
    let mut out = String::new();
    for (name, value) in pairs {
        out += &format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n{value}\r\n",
            quote(name)
        );
    }
    for name in files {
        out += &format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n\r\n",
            quote(name)
        );
    }
    out += &format!("--{boundary}--\r\n");
    net::Body {
        content_type: format!("multipart/form-data; boundary={boundary}"),
        bytes: out.into_bytes(),
    }
}

/// The `<form>` a control belongs to: the one its `form` attribute names, else the nearest
/// ancestor.
fn form_of(dom: &Dom, node: NodeId) -> Option<NodeId> {
    if let Some(id) = dom.attr(node, "form").filter(|v| !v.is_empty()) {
        return dom
            .descendants(Dom::ROOT)
            .find(|&n| dom.tag_name(n) == Some("form") && dom.attr(n, "id") == Some(id));
    }
    let mut n = dom.parent(node);
    while let Some(id) = n {
        if dom.tag_name(id) == Some("form") {
            return Some(id);
        }
        n = dom.parent(id);
    }
    None
}

fn same_group(dom: &Dom, a: &Control, b: &Control) -> bool {
    let name = |c: &Control| dom.attr(c.node, "name").filter(|n| !n.is_empty());
    b.kind == Kind::Radio
        && name(a).is_some()
        && name(a) == name(b)
        && form_of(dom, a.node) == form_of(dom, b.node)
}

/// `application/x-www-form-urlencoded`: UTF-8, spaces as `+`, everything but letters,
/// digits and `*-._` percent-encoded.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
pub fn urlencode(pairs: &[(String, String)]) -> String {
    fn enc(out: &mut String, s: &str) {
        for b in s.bytes() {
            match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                    out.push(b as char)
                }
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
    }
    let mut out = String::new();
    for (k, (name, value)) in pairs.iter().enumerate() {
        if k > 0 {
            out.push('&');
        }
        enc(&mut out, name);
        out.push('=');
        enc(&mut out, value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(src: &str) -> (Dom, Forms) {
        let mut dom = html::parse(src);
        let forms = Forms::prepare(&mut dom);
        (dom, forms)
    }

    fn base() -> net::Url {
        net::Url::parse("https://example.org/search/page?old=1#top").unwrap()
    }

    #[test]
    fn validates_before_sending() {
        let (mut dom, mut forms) = page(
            "<form><input name=a required><input type=email name=e><input type=url name=u>\
             <input type=number name=n min=1 max=5><input type=checkbox name=c required>\
             <select name=s required><option value=''>Pick</option><option>x</option></select>\
             <input type=radio name=r value=1 required><input type=radio name=r value=2>\
             <button>Go</button></form><form novalidate><input required><button>Go</button></form>",
        );
        let go = 8;
        assert_eq!(
            forms.invalid(&dom, go),
            Some((0, "Please fill in this field."))
        );
        forms.set_value(&mut dom, 0, "x");
        forms.set_value(&mut dom, 1, "not an address");
        assert_eq!(
            forms.invalid(&dom, go),
            Some((1, "Please enter an email address."))
        );
        forms.set_value(&mut dom, 1, "me@example.org");
        forms.set_value(&mut dom, 2, "example");
        assert_eq!(forms.invalid(&dom, go).map(|v| v.0), Some(2));
        forms.set_value(&mut dom, 2, "https://example.org/");
        forms.set_value(&mut dom, 3, "9");
        assert_eq!(forms.invalid(&dom, go).map(|v| v.0), Some(3), "above max");
        forms.set_value(&mut dom, 3, "3");
        assert_eq!(forms.invalid(&dom, go), Some((4, "Please check this box.")));
        forms.toggle(&mut dom, 4);
        assert_eq!(
            forms.invalid(&dom, go),
            Some((5, "Please choose an option."))
        );
        forms.choose_index(&mut dom, 5, 1);
        assert_eq!(
            forms.invalid(&dom, go).map(|v| v.0),
            Some(6),
            "no radio of the group"
        );
        forms.toggle(&mut dom, 7);
        assert_eq!(
            forms.invalid(&dom, go),
            None,
            "another radio of the group counts"
        );
        assert_eq!(forms.invalid(&dom, 10), None, "novalidate");

        assert_eq!(forms.stepped(&dom, 3, true).as_deref(), Some("4"));
        forms.set_value(&mut dom, 3, "5");
        assert_eq!(
            forms.stepped(&dom, 3, true).as_deref(),
            Some("5"),
            "stays within max"
        );
        assert_eq!(
            forms.stepped(&dom, 0, true),
            None,
            "only number fields step"
        );
    }

    #[test]
    fn sends_multipart_with_empty_file_parts() {
        let (mut dom, mut forms) = page(
            "<form method=post enctype=multipart/form-data action=/up>\
             <input name='a\"b' value=1><textarea name=t></textarea><input type=file name=f>\
             <button>Go</button></form>",
        );
        forms.set_value(&mut dom, 1, "two\nlines");
        let sent = forms.submission(&dom, 3, &base()).unwrap();
        let body = sent.body.unwrap();
        let boundary = body
            .content_type
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap()
            .to_string();
        let text = String::from_utf8(body.bytes).unwrap();
        assert_eq!(
            text,
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"a%22b\"\r\n\r\n1\r\n\
                 --{boundary}\r\nContent-Disposition: form-data; name=\"t\"\r\n\r\ntwo\r\nlines\r\n\
                 --{boundary}\r\nContent-Disposition: form-data; name=\"f\"; filename=\"\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n\r\n--{boundary}--\r\n"
            )
        );
        assert!(is_email("a@b.c") && !is_email("a@") && !is_email("a b@c.d") && !is_email("@c"));
    }

    #[test]
    fn controls_show_values_placeholders_and_labels() {
        let (dom, forms) = page(
            "<input name=q placeholder='Search'><input type=password value=abc>\
             <input type=submit><textarea>\nhi</textarea>\
             <select><option>One<option selected> Two  words </select><input type=hidden>",
        );
        let kinds: Vec<Kind> = forms.controls.iter().map(|c| c.kind).collect();
        use Kind::*;
        assert_eq!(kinds, [Text, Password, Submit, TextArea, Select, Hidden]);
        let text = |i: usize| dom.text_content(forms.controls[i].node);
        assert_eq!(text(0), "Search", "placeholder while empty");
        assert_eq!(text(1), "•••", "passwords are never shown");
        assert_eq!(text(2), "Submit");
        assert_eq!(text(3), "hi");
        assert_eq!(forms.controls[3].value, "hi", "leading newline dropped");
        assert!(
            text(4).starts_with("One"),
            "options stay in the DOM, hidden by ua.css"
        );
        assert!(
            text(4).ends_with("Two words▼"),
            "the chosen label, then the arrow"
        );
    }

    #[test]
    fn editing_toggling_and_choosing() {
        let (mut dom, mut forms) = page(
            "<form><input name=q placeholder=Find><input type=checkbox name=c>\
             <input type=radio name=r value=a checked><input type=radio name=r value=b>\
             <select name=s><option>x<option>y</select></form>",
        );
        forms.set_value(&mut dom, 0, "rust\nlang");
        assert_eq!(dom.text_content(forms.controls[0].node), "rustlang");
        forms.set_value(&mut dom, 0, "");
        assert_eq!(dom.text_content(forms.controls[0].node), "Find");
        assert!(forms.toggle(&mut dom, 1));
        assert!(forms.controls[1].checked);
        let mark = |forms: &Forms, i: usize| {
            let m = forms.controls[i].mark.unwrap();
            dom.tag_name(m).map(str::to_string)
        };
        assert_eq!(mark(&forms, 1).as_deref(), Some("broxee-check"));
        assert!(forms.toggle(&mut dom, 3), "the other radio button");
        assert!(!forms.controls[2].checked && forms.controls[3].checked);
        assert!(!forms.toggle(&mut dom, 3), "a checked radio stays checked");
        assert!(forms.choose(&mut dom, 4, 1));
        assert!(forms.choose(&mut dom, 4, 1), "wraps around");
        assert_eq!(forms.controls[4].selected, 0);
        assert_eq!(forms.at(&dom, forms.controls[0].shown.unwrap()), Some(0));
    }

    #[test]
    fn submits_get_and_post() {
        let (mut dom, mut forms) = page(
            "<form action='/find'><input name=q><input type=checkbox name=safe checked>\
             <input type=checkbox name=off><input name=nameless value=x><input name=d disabled value=y>\
             <select name=lang><option value=en>English<option>Bahasa Indonesia</select>\
             <input type=submit name=go value=Go><input type=submit name=other value=No></form>\
             <form method=POST><textarea name=t>a\nb</textarea><input type=password name=p></form>\
             <input name=outside>",
        );
        forms.set_value(&mut dom, 0, "café & tea");
        forms.choose(&mut dom, 5, 1);
        let s = forms.submission(&dom, 0, &base()).unwrap();
        assert_eq!(
            s.url.to_string(),
            "https://example.org/find?q=caf%C3%A9+%26+tea&safe=on&nameless=x&lang=Bahasa+Indonesia&go=Go",
            "Enter presses the first submit button"
        );
        assert!(s.body.is_none());
        let s = forms.submission(&dom, 7, &base()).unwrap();
        assert!(
            s.url.to_string().ends_with("&other=No"),
            "the button pressed"
        );

        forms.set_value(&mut dom, 9, "s3cret");
        let s = forms.submission(&dom, 9, &base()).unwrap();
        assert_eq!(s.url.to_string(), "https://example.org/search/page?old=1");
        let body = s.body.unwrap();
        assert_eq!(body.content_type, "application/x-www-form-urlencoded");
        assert_eq!(body.bytes, b"t=a%0D%0Ab&p=s3cret");
        assert_eq!(forms.submission(&dom, 10, &base()), None, "not in a form");
    }

    #[test]
    fn reset_brings_back_the_page_values() {
        let (mut dom, mut forms) = page(
            "<form><input name=a value=one><input type=checkbox checked><input type=reset></form>",
        );
        forms.set_value(&mut dom, 0, "two");
        forms.toggle(&mut dom, 1);
        forms.reset(&mut dom, 2);
        assert_eq!(forms.controls[0].value, "one");
        assert!(forms.controls[1].checked);
    }

    #[test]
    fn rebuilding_gives_the_same_dom() {
        let src = "<p>x</p><input name=q value=v><select><option>a</select><img src=a.png>";
        let (a, _) = page(src);
        let (b, _) = page(src);
        assert_eq!(a.len(), b.len());
        assert_eq!(a.dump(10), b.dump(10));
        let img = a.find_first("img").unwrap();
        assert_eq!(
            html::parse(src).find_first("img"),
            Some(img),
            "page nodes keep their ids"
        );
    }
}
