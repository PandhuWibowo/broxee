//! Tokens of JavaScript source. Regular expression literals are recognised from context
//! (after something that cannot end an expression) so that `/` divides everywhere else.

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Num(f64),
    Str(String),
    /// A template literal's raw text between the backticks; the parser splits `${…}`.
    Template(String),
    Regex(String),
    Ident(String),
    Punct(&'static str),
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    /// A line break came before this token (for automatic semicolons).
    pub newline_before: bool,
    pub line: u32,
}

const PUNCTS: &[&str] = &[
    ">>>=", "...", "===", "!==", "**=", "<<=", ">>=", ">>>", "&&=", "||=", "??=", "=>", "==", "!=",
    "<=", ">=", "&&", "||", "??", "?.", "++", "--", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=",
    "<<", ">>", "**", "{", "}", "(", ")", "[", "]", ";", ",", "<", ">", "+", "-", "*", "/", "%",
    "&", "|", "^", "!", "~", "?", ":", "=", ".", "@", "#",
];

pub fn tokenize(src: &str) -> Result<Vec<Token>, String> {
    let b = src.as_bytes();
    let mut out: Vec<Token> = Vec::new();
    let mut i = 0;
    let mut line = 1u32;
    let mut newline = false;
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            line += 1;
            newline = true;
            i += 1;
            continue;
        }
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // Non-ASCII whitespace (no-break space, line separators).
        if c >= 0x80 {
            let ch = src[i..].chars().next().unwrap_or(' ');
            if ch.is_whitespace() || ch == '\u{feff}' {
                if ch == '\u{2028}' || ch == '\u{2029}' {
                    newline = true;
                }
                i += ch.len_utf8();
                continue;
            }
        }
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let end = src[i + 2..].find("*/").map_or(b.len(), |e| i + 2 + e + 2);
            let skipped = &src[i..end];
            let lines = skipped.matches('\n').count() as u32;
            if lines > 0 {
                newline = true;
                line += lines;
            }
            i = end;
            continue;
        }
        // An HTML comment opener at the start of a script is treated as a line comment.
        if src[i..].starts_with("<!--")
            || (newline || out.is_empty()) && src[i..].starts_with("-->")
        {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        let tok = if c.is_ascii_digit()
            || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            let (v, end) = number(src, i)?;
            i = end;
            Tok::Num(v)
        } else if c == b'"' || c == b'\'' {
            let (s, end, lines) = string(src, i)?;
            line += lines;
            i = end;
            Tok::Str(s)
        } else if c == b'`' {
            let mut j = i + 1;
            let mut depth = 0usize;
            while j < b.len() {
                match b[j] {
                    b'\\' => j += 1,
                    b'`' if depth == 0 => break,
                    b'$' if b.get(j + 1) == Some(&b'{') => {
                        depth += 1;
                        j += 1;
                    }
                    b'}' if depth > 0 => depth -= 1,
                    b'\n' => line += 1,
                    _ => {}
                }
                j += 1;
            }
            if j >= b.len() {
                return Err(format!("line {line}: unterminated template literal"));
            }
            let raw = src[i + 1..j].to_string();
            i = j + 1;
            Tok::Template(raw)
        } else if c == b'/' && regex_allowed(out.last().map(|t| &t.tok)) {
            let mut j = i + 1;
            let mut class = false;
            while j < b.len() {
                match b[j] {
                    b'\\' => j += 1,
                    b'[' => class = true,
                    b']' => class = false,
                    b'/' if !class => break,
                    b'\n' => return Err(format!("line {line}: unterminated regular expression")),
                    _ => {}
                }
                j += 1;
            }
            j += 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            let raw = src[i..j.min(b.len())].to_string();
            i = j;
            Tok::Regex(raw)
        } else if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80 || c == b'\\' {
            let mut j = i;
            while j < b.len() {
                let ch = src[j..].chars().next().unwrap_or(' ');
                if ch.is_alphanumeric()
                    || ch == '_'
                    || ch == '$'
                    || ch == '\u{200c}'
                    || ch == '\u{200d}'
                {
                    j += ch.len_utf8();
                } else {
                    break;
                }
            }
            if j == i {
                return Err(format!(
                    "line {line}: unexpected character {:?}",
                    src[i..].chars().next()
                ));
            }
            let name = src[i..j].to_string();
            i = j;
            Tok::Ident(name)
        } else {
            let p = PUNCTS
                .iter()
                // `a?.5:1` is a conditional, not optional chaining.
                .filter(|p| **p != "?." || !b.get(i + 2).is_some_and(u8::is_ascii_digit))
                .find(|p| src[i..].starts_with(**p))
                .ok_or_else(|| format!("line {line}: unexpected character {:?}", c as char))?;
            i += p.len();
            Tok::Punct(p)
        };
        let _ = start;
        out.push(Token {
            tok,
            newline_before: std::mem::take(&mut newline),
            line,
        });
    }
    out.push(Token {
        tok: Tok::Eof,
        newline_before: true,
        line,
    });
    Ok(out)
}

/// Whether a `/` after `prev` starts a regular expression rather than dividing.
fn regex_allowed(prev: Option<&Tok>) -> bool {
    match prev {
        None => true,
        Some(Tok::Num(_) | Tok::Str(_) | Tok::Template(_) | Tok::Regex(_)) => false,
        Some(Tok::Ident(w)) => matches!(
            w.as_str(),
            "return"
                | "typeof"
                | "instanceof"
                | "in"
                | "of"
                | "new"
                | "delete"
                | "void"
                | "throw"
                | "case"
                | "do"
                | "else"
        ),
        Some(Tok::Punct(p)) => !matches!(*p, ")" | "]" | "}" | "++" | "--"),
        Some(Tok::Eof) => true,
    }
}

fn number(src: &str, start: usize) -> Result<(f64, usize), String> {
    let b = src.as_bytes();
    let mut i = start;
    if b[i] == b'0' && matches!(b.get(i + 1), Some(b'x' | b'X' | b'o' | b'O' | b'b' | b'B')) {
        let radix = match b[i + 1] {
            b'x' | b'X' => 16,
            b'o' | b'O' => 8,
            _ => 2,
        };
        i += 2;
        let digits_start = i;
        while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
            i += 1;
        }
        let digits: String = src[digits_start..i]
            .chars()
            .filter(|&c| c != '_' && c != 'n')
            .collect();
        let v = u64::from_str_radix(&digits, radix).map_err(|_| "bad number".to_string())?;
        return Ok((v as f64, i));
    }
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
            i += 1;
        }
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    let text: String = src[start..i].chars().filter(|&c| c != '_').collect();
    // BigInt literals (`10n`) read as numbers.
    if i < b.len() && b[i] == b'n' {
        i += 1;
    }
    let v = text
        .parse::<f64>()
        .map_err(|_| format!("bad number {text}"))?;
    Ok((v, i))
}

/// A quoted string from `start` (the quote). Returns its value, the end, and line breaks
/// consumed by line continuations.
fn string(src: &str, start: usize) -> Result<(String, usize, u32), String> {
    let quote = src.as_bytes()[start] as char;
    let mut out = String::new();
    let mut chars = src[start + 1..].char_indices();
    let mut lines = 0;
    while let Some((k, ch)) = chars.next() {
        match ch {
            c if c == quote => return Ok((out, start + 1 + k + 1, lines)),
            '\n' => return Err("unterminated string".into()),
            '\\' => {
                let Some((_, e)) = chars.next() else { break };
                match e {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'v' => out.push('\u{b}'),
                    '0' => out.push('\0'),
                    '\n' => lines += 1,
                    'x' => {
                        let h: String = (0..2).filter_map(|_| chars.next().map(|c| c.1)).collect();
                        out.push(
                            u32::from_str_radix(&h, 16)
                                .ok()
                                .and_then(char::from_u32)
                                .unwrap_or('\u{fffd}'),
                        );
                    }
                    'u' => out.push(unicode_escape(&mut chars)),
                    other => out.push(other),
                }
            }
            c => out.push(c),
        }
    }
    Err("unterminated string".into())
}

pub fn unicode_escape(chars: &mut std::str::CharIndices) -> char {
    let mut peek = chars.clone();
    let code = if peek.next().map(|c| c.1) == Some('{') {
        chars.next();
        let mut h = String::new();
        for (_, c) in chars.by_ref() {
            if c == '}' {
                break;
            }
            h.push(c);
        }
        u32::from_str_radix(&h, 16).unwrap_or(0xfffd)
    } else {
        let h: String = (0..4).filter_map(|_| chars.next().map(|c| c.1)).collect();
        let hi = u32::from_str_radix(&h, 16).unwrap_or(0xfffd);
        // A surrogate pair written as two escapes.
        if (0xd800..0xdc00).contains(&hi) {
            let mut look = chars.clone();
            if look.next().map(|c| c.1) == Some('\\') && look.next().map(|c| c.1) == Some('u') {
                let h2: String = (0..4).filter_map(|_| look.next().map(|c| c.1)).collect();
                if let Ok(lo) = u32::from_str_radix(&h2, 16) {
                    if (0xdc00..0xe000).contains(&lo) {
                        *chars = look;
                        return char::from_u32(0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00))
                            .unwrap_or('\u{fffd}');
                    }
                }
            }
        }
        hi
    };
    char::from_u32(code).unwrap_or('\u{fffd}')
}
