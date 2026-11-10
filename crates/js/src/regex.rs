//! A small backtracking regular expression engine for script `RegExp`s: literals, `.`,
//! classes with ranges and negation, `\d \w \s \b` and their negations, `^ $`, groups
//! (capturing and `(?:`), alternation, greedy and lazy quantifiers, and the `g i m s` flags.
//! Lookaround and backreferences are refused, so a script using them fails visibly.
//! Matching works on chars and gives up past a step budget.

#[derive(Debug, Clone)]
enum Node {
    Char(char),
    Any,
    Class(Vec<ClassItem>, bool),
    Start,
    End,
    WordBoundary(bool),
    Group(Box<Node>, Option<usize>),
    Alt(Vec<Node>),
    Concat(Vec<Node>),
    Repeat(Box<Node>, usize, usize, bool),
}

#[derive(Debug, Clone)]
enum ClassItem {
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

#[derive(Debug)]
pub struct Regex {
    node: Node,
    pub groups: usize,
    pub global: bool,
    ignore_case: bool,
    multiline: bool,
    dot_all: bool,
}

const STEP_BUDGET: usize = 2_000_000;

struct P<'a> {
    c: Vec<char>,
    i: usize,
    groups: usize,
    _s: &'a str,
}

impl Regex {
    pub fn new(pattern: &str, flags: &str) -> Result<Regex, String> {
        let mut p = P {
            c: pattern.chars().collect(),
            i: 0,
            groups: 0,
            _s: pattern,
        };
        let node = p.alt()?;
        if p.i < p.c.len() {
            return Err("unbalanced parenthesis".into());
        }
        Ok(Regex {
            node,
            groups: p.groups,
            global: flags.contains('g'),
            ignore_case: flags.contains('i'),
            multiline: flags.contains('m'),
            dot_all: flags.contains('s'),
        })
    }

    /// The first match at or after char index `start`: the span of the whole match and of
    /// each group, as char indices.
    pub fn find_at(&self, text: &[char], start: usize) -> Option<Vec<Option<(usize, usize)>>> {
        let mut steps = 0;
        for s in start..=text.len() {
            let mut caps = vec![None; self.groups + 1];
            let mut m = Matcher {
                re: self,
                text,
                steps: &mut steps,
            };
            if let Some(end) = m.run(&self.node, s, &mut caps, &mut |_, e, _| Some(e)) {
                caps[0] = Some((s, end));
                return Some(caps);
            }
            if steps > STEP_BUDGET {
                return None;
            }
        }
        None
    }
}

impl P<'_> {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }

    fn alt(&mut self) -> Result<Node, String> {
        let mut branches = vec![self.concat()?];
        while self.peek() == Some('|') {
            self.i += 1;
            branches.push(self.concat()?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().unwrap_or(Node::Concat(Vec::new()))
        } else {
            Node::Alt(branches)
        })
    }

    fn concat(&mut self) -> Result<Node, String> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.atom()?;
            let atom = self.quantifier(atom)?;
            items.push(atom);
        }
        Ok(Node::Concat(items))
    }

    fn number(&mut self) -> Option<usize> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        self.c[start..self.i]
            .iter()
            .collect::<String>()
            .parse()
            .ok()
    }

    fn quantifier(&mut self, atom: Node) -> Result<Node, String> {
        let (min, max) = match self.peek() {
            Some('*') => (0, usize::MAX),
            Some('+') => (1, usize::MAX),
            Some('?') => (0, 1),
            Some('{') => {
                let save = self.i;
                self.i += 1;
                let Some(min) = self.number() else {
                    self.i = save;
                    return Ok(atom);
                };
                let max = if self.peek() == Some(',') {
                    self.i += 1;
                    self.number().unwrap_or(usize::MAX)
                } else {
                    min
                };
                if self.peek() != Some('}') {
                    self.i = save;
                    return Ok(atom);
                }
                (min, max)
            }
            _ => return Ok(atom),
        };
        self.i += 1;
        let lazy = self.peek() == Some('?');
        if lazy {
            self.i += 1;
        }
        Ok(Node::Repeat(Box::new(atom), min, max, !lazy))
    }

    fn escape(&mut self) -> Result<ClassOrNode, String> {
        let c = self.peek().ok_or("trailing backslash")?;
        self.i += 1;
        Ok(match c {
            'd' => ClassOrNode::Class(ClassItem::Digit(true)),
            'D' => ClassOrNode::Class(ClassItem::Digit(false)),
            'w' => ClassOrNode::Class(ClassItem::Word(true)),
            'W' => ClassOrNode::Class(ClassItem::Word(false)),
            's' => ClassOrNode::Class(ClassItem::Space(true)),
            'S' => ClassOrNode::Class(ClassItem::Space(false)),
            'b' => ClassOrNode::Node(Node::WordBoundary(true)),
            'B' => ClassOrNode::Node(Node::WordBoundary(false)),
            'n' => ClassOrNode::Char('\n'),
            't' => ClassOrNode::Char('\t'),
            'r' => ClassOrNode::Char('\r'),
            'f' => ClassOrNode::Char('\u{c}'),
            'v' => ClassOrNode::Char('\u{b}'),
            '0' => ClassOrNode::Char('\0'),
            'x' | 'u' => {
                let n = if c == 'x' { 2 } else { 4 };
                let h: String = self.c[self.i..(self.i + n).min(self.c.len())]
                    .iter()
                    .collect();
                self.i += h.len();
                ClassOrNode::Char(
                    u32::from_str_radix(&h, 16)
                        .ok()
                        .and_then(char::from_u32)
                        .unwrap_or('\u{fffd}'),
                )
            }
            '1'..='9' => return Err("backreferences are not supported".into()),
            other => ClassOrNode::Char(other),
        })
    }

    fn atom(&mut self) -> Result<Node, String> {
        let c = self.peek().ok_or("unexpected end")?;
        self.i += 1;
        Ok(match c {
            '.' => Node::Any,
            '^' => Node::Start,
            '$' => Node::End,
            '(' => {
                let index = if self.c.get(self.i..self.i + 2) == Some(&['?', ':']) {
                    self.i += 2;
                    None
                } else if self.peek() == Some('?') {
                    return Err("lookaround is not supported".into());
                } else {
                    self.groups += 1;
                    Some(self.groups)
                };
                let inner = self.alt()?;
                if self.peek() != Some(')') {
                    return Err("missing )".into());
                }
                self.i += 1;
                Node::Group(Box::new(inner), index)
            }
            '[' => {
                let negate = self.peek() == Some('^');
                if negate {
                    self.i += 1;
                }
                let mut items = Vec::new();
                let mut first = true;
                loop {
                    let c = self.peek().ok_or("missing ]")?;
                    if c == ']' && !first {
                        self.i += 1;
                        break;
                    }
                    first = false;
                    self.i += 1;
                    let lo = if c == '\\' {
                        match self.escape()? {
                            ClassOrNode::Char(ch) => ch,
                            ClassOrNode::Class(item) => {
                                items.push(item);
                                continue;
                            }
                            ClassOrNode::Node(_) => '\u{8}',
                        }
                    } else {
                        c
                    };
                    if self.peek() == Some('-') && self.c.get(self.i + 1).is_some_and(|&n| n != ']')
                    {
                        self.i += 1;
                        let h = self.peek().ok_or("missing ]")?;
                        self.i += 1;
                        let hi = if h == '\\' {
                            match self.escape()? {
                                ClassOrNode::Char(ch) => ch,
                                _ => h,
                            }
                        } else {
                            h
                        };
                        items.push(ClassItem::Range(lo, hi));
                    } else {
                        items.push(ClassItem::Range(lo, lo));
                    }
                }
                Node::Class(items, negate)
            }
            '\\' => match self.escape()? {
                ClassOrNode::Char(ch) => Node::Char(ch),
                ClassOrNode::Class(item) => Node::Class(vec![item], false),
                ClassOrNode::Node(n) => n,
            },
            '*' | '+' | '?' => return Err("nothing to repeat".into()),
            other => Node::Char(other),
        })
    }
}

enum ClassOrNode {
    Char(char),
    Class(ClassItem),
    Node(Node),
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

type Caps = Vec<Option<(usize, usize)>>;

struct Matcher<'a> {
    re: &'a Regex,
    text: &'a [char],
    steps: &'a mut usize,
}

impl Matcher<'_> {
    fn eq(&self, a: char, b: char) -> bool {
        a == b || self.re.ignore_case && a.to_lowercase().eq(b.to_lowercase())
    }

    fn class_has(&self, items: &[ClassItem], c: char) -> bool {
        items.iter().any(|item| match *item {
            ClassItem::Range(lo, hi) => {
                (lo..=hi).contains(&c)
                    || self.re.ignore_case && {
                        let l = c.to_ascii_lowercase();
                        let u = c.to_ascii_uppercase();
                        (lo..=hi).contains(&l) || (lo..=hi).contains(&u)
                    }
            }
            ClassItem::Digit(yes) => c.is_ascii_digit() == yes,
            ClassItem::Word(yes) => is_word(c) == yes,
            ClassItem::Space(yes) => c.is_whitespace() == yes,
        })
    }

    /// Match `node` at `i`, then the continuation `k`; returns the end of the whole match.
    fn run(
        &mut self,
        node: &Node,
        i: usize,
        caps: &mut Caps,
        k: &mut dyn FnMut(&mut Self, usize, &mut Caps) -> Option<usize>,
    ) -> Option<usize> {
        *self.steps += 1;
        if *self.steps > STEP_BUDGET {
            return None;
        }
        let t = self.text;
        match node {
            Node::Char(c) => (i < t.len() && self.eq(t[i], *c))
                .then(|| k(self, i + 1, caps))
                .flatten(),
            Node::Any => (i < t.len() && (self.re.dot_all || t[i] != '\n'))
                .then(|| k(self, i + 1, caps))
                .flatten(),
            Node::Class(items, negate) => (i < t.len() && self.class_has(items, t[i]) != *negate)
                .then(|| k(self, i + 1, caps))
                .flatten(),
            Node::Start => (i == 0 || self.re.multiline && t[i - 1] == '\n')
                .then(|| k(self, i, caps))
                .flatten(),
            Node::End => (i == t.len() || self.re.multiline && t[i] == '\n')
                .then(|| k(self, i, caps))
                .flatten(),
            Node::WordBoundary(want) => {
                let before = i > 0 && is_word(t[i - 1]);
                let after = i < t.len() && is_word(t[i]);
                ((before != after) == *want)
                    .then(|| k(self, i, caps))
                    .flatten()
            }
            Node::Group(inner, index) => {
                let index = *index;
                self.run(inner, i, caps, &mut |m, end, caps| {
                    let saved = index.map(|g| caps[g]);
                    if let Some(g) = index {
                        caps[g] = Some((i, end));
                    }
                    let r = k(m, end, caps);
                    if r.is_none() {
                        if let (Some(g), Some(s)) = (index, saved) {
                            caps[g] = s;
                        }
                    }
                    r
                })
            }
            Node::Alt(branches) => {
                for b in branches {
                    if let Some(r) = self.run(b, i, caps, k) {
                        return Some(r);
                    }
                }
                None
            }
            Node::Concat(items) => self.concat(items, i, caps, k),
            Node::Repeat(inner, min, max, greedy) => {
                self.repeat(inner, *min, *max, *greedy, i, 0, caps, k)
            }
        }
    }

    fn concat(
        &mut self,
        items: &[Node],
        i: usize,
        caps: &mut Caps,
        k: &mut dyn FnMut(&mut Self, usize, &mut Caps) -> Option<usize>,
    ) -> Option<usize> {
        match items.split_first() {
            None => k(self, i, caps),
            Some((first, rest)) => {
                self.run(first, i, caps, &mut |m, j, caps| m.concat(rest, j, caps, k))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn repeat(
        &mut self,
        inner: &Node,
        min: usize,
        max: usize,
        greedy: bool,
        i: usize,
        count: usize,
        caps: &mut Caps,
        k: &mut dyn FnMut(&mut Self, usize, &mut Caps) -> Option<usize>,
    ) -> Option<usize> {
        if count < min {
            return self.repeat_more(inner, min, max, greedy, i, count, caps, k);
        }
        if greedy {
            if let Some(r) = self.repeat_more(inner, min, max, greedy, i, count, caps, k) {
                return Some(r);
            }
            k(self, i, caps)
        } else {
            if let Some(r) = k(self, i, caps) {
                return Some(r);
            }
            self.repeat_more(inner, min, max, greedy, i, count, caps, k)
        }
    }

    /// One more repetition of `inner` at `i`, then the rest of the repeat.
    #[allow(clippy::too_many_arguments)]
    fn repeat_more(
        &mut self,
        inner: &Node,
        min: usize,
        max: usize,
        greedy: bool,
        i: usize,
        count: usize,
        caps: &mut Caps,
        k: &mut dyn FnMut(&mut Self, usize, &mut Caps) -> Option<usize>,
    ) -> Option<usize> {
        if count >= max {
            return None;
        }
        self.run(inner, i, caps, &mut |m, j, caps| {
            // An empty match cannot repeat forever.
            if j == i && count >= min {
                return None;
            }
            m.repeat(inner, min, max, greedy, j, count + 1, caps, k)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(p: &str, flags: &str, text: &str) -> Option<String> {
        let re = Regex::new(p, flags).unwrap();
        let chars: Vec<char> = text.chars().collect();
        let caps = re.find_at(&chars, 0)?;
        let (s, e) = caps[0]?;
        Some(chars[s..e].iter().collect())
    }

    #[test]
    fn matches_the_usual_patterns() {
        assert_eq!(find(r"\d+", "", "abc 123 x").as_deref(), Some("123"));
        assert_eq!(find(r"^\s+|\s+$", "", "  hi ").as_deref(), Some("  "));
        assert_eq!(
            find(r"[a-z]+@[a-z]+\.com", "", "mail me@site.com now").as_deref(),
            Some("me@site.com")
        );
        assert_eq!(find(r"colou?r", "", "the color").as_deref(), Some("color"));
        assert_eq!(find(r"a{2,3}", "", "caaaat").as_deref(), Some("aaa"));
        assert_eq!(find(r"<.+?>", "", "<a><b>").as_deref(), Some("<a>"));
        assert_eq!(find(r"HELLO", "i", "say hello").as_deref(), Some("hello"));
        assert_eq!(find(r"\bcat\b", "", "concat cat").as_deref(), Some("cat"));
        assert_eq!(find(r"(?:ab)+", "", "xababy").as_deref(), Some("abab"));
        assert_eq!(find(r"[^\s]+$", "", "one two").as_deref(), Some("two"));
        assert_eq!(find(r"x", "", "abc"), None);
        let re = Regex::new(r"(\w+)-(\d+)", "").unwrap();
        let text: Vec<char> = "id item-42".chars().collect();
        let caps = re.find_at(&text, 0).unwrap();
        assert_eq!(caps[1], Some((3, 7)));
        assert_eq!(caps[2], Some((8, 10)));
        assert!(Regex::new("(?=x)", "").is_err());
        assert!(Regex::new("(a", "").is_err());
        // Catastrophic patterns give up instead of hanging.
        let _ = find("(a*)*b", "", &"a".repeat(40));
    }
}
