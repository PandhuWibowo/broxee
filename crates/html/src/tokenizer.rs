use std::borrow::Cow;

use crate::entities::decode_entities;

/// Tokens borrow from the source wherever possible. An owned string is created only when the
/// token differs from the source: uppercase names that need lowercasing, or decoded entities.
#[derive(Debug, PartialEq)]
pub enum Token<'a> {
    Doctype(Cow<'a, str>),
    Comment(Cow<'a, str>),
    Text(Cow<'a, str>),
    StartTag {
        tag: Cow<'a, str>,
        attrs: Vec<(Cow<'a, str>, Cow<'a, str>)>,
        self_closing: bool,
    },
    EndTag(Cow<'a, str>),
}

/// Byte-oriented tokenizer. Every slice boundary it uses falls on an ASCII
/// delimiter, so slicing the UTF-8 source is always valid.
pub struct Tokenizer<'a> {
    src: &'a str,
    pos: usize,
    /// Set after `<script>`, `<style>`, `<title>`, `<textarea>`: content runs
    /// verbatim until the matching end tag. The flag says whether to decode entities.
    raw_text: Option<(&'static str, bool)>,
}

impl<'a> Tokenizer<'a> {
    pub fn new(src: &'a str) -> Self {
        Tokenizer {
            src,
            pos: 0,
            raw_text: None,
        }
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn start_tag(&mut self) -> Token<'a> {
        let src = self.rest();
        let s = src.as_bytes();
        let stop = |b: u8| b.is_ascii_whitespace() || b == b'>' || b == b'/';

        let mut i = 1;
        while i < s.len() && !stop(s[i]) {
            i += 1;
        }
        let tag = lowercase(&src[1..i]);

        let mut attrs: Vec<(Cow<'a, str>, Cow<'a, str>)> = Vec::new();
        let mut self_closing = false;
        loop {
            while i < s.len() && s[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= s.len() {
                break;
            }
            if s[i] == b'>' {
                i += 1;
                break;
            }
            if s[i] == b'/' {
                i += 1;
                if s.get(i) == Some(&b'>') {
                    self_closing = true;
                    i += 1;
                    break;
                }
                continue;
            }

            let name_start = i;
            while i < s.len() && !stop(s[i]) && s[i] != b'=' {
                i += 1;
            }
            let name = lowercase(&src[name_start..i]);
            while i < s.len() && s[i].is_ascii_whitespace() {
                i += 1;
            }

            let mut value = Cow::Borrowed("");
            if s.get(i) == Some(&b'=') {
                i += 1;
                while i < s.len() && s[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i < s.len() && (s[i] == b'"' || s[i] == b'\'') {
                    let quote = s[i];
                    i += 1;
                    let start = i;
                    while i < s.len() && s[i] != quote {
                        i += 1;
                    }
                    value = decode_entities(&src[start..i]);
                    i = (i + 1).min(s.len());
                } else {
                    let start = i;
                    while i < s.len() && !s[i].is_ascii_whitespace() && s[i] != b'>' {
                        i += 1;
                    }
                    value = decode_entities(&src[start..i]);
                }
            }
            // First occurrence wins, as in the HTML spec.
            if !name.is_empty() && !attrs.iter().any(|(k, _)| *k == name) {
                attrs.push((name, value));
            }
        }

        self.pos += i;
        if !self_closing {
            self.raw_text = match &*tag {
                "script" => Some(("script", false)),
                "style" => Some(("style", false)),
                "title" => Some(("title", true)),
                "textarea" => Some(("textarea", true)),
                _ => None,
            };
        }
        Token::StartTag {
            tag,
            attrs,
            self_closing,
        }
    }
}

impl<'a> Iterator for Tokenizer<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        if let Some((tag, decode)) = self.raw_text.take() {
            let rest = self.rest();
            let end = find_end_tag(rest, tag).unwrap_or(rest.len());
            self.pos += end;
            if end > 0 {
                let text = &rest[..end];
                return Some(Token::Text(if decode {
                    decode_entities(text)
                } else {
                    Cow::Borrowed(text)
                }));
            }
        }

        let rest = self.rest();
        let b = rest.as_bytes();
        if b.is_empty() {
            return None;
        }

        if b[0] == b'<' {
            if let Some(body) = rest.strip_prefix("<!--") {
                let (comment, consumed) = match body.find("-->") {
                    Some(i) => (&body[..i], 4 + i + 3),
                    None => (body, rest.len()),
                };
                self.pos += consumed;
                return Some(Token::Comment(Cow::Borrowed(comment)));
            }
            if matches!(b.get(1), Some(b'!' | b'?')) {
                let end = rest.find('>').map_or(rest.len(), |i| i + 1);
                self.pos += end;
                let inner = Cow::Borrowed(rest[2..end].trim_end_matches('>').trim());
                return Some(if b[1] == b'!' {
                    Token::Doctype(inner)
                } else {
                    Token::Comment(inner)
                });
            }
            if b.get(1) == Some(&b'/') && b.get(2).is_some_and(u8::is_ascii_alphabetic) {
                let end = rest.find('>').map_or(rest.len(), |i| i + 1);
                let name = rest[2..end]
                    .split(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/')
                    .next()
                    .unwrap_or_default();
                self.pos += end;
                return Some(Token::EndTag(lowercase(name)));
            }
            if b.get(1).is_some_and(u8::is_ascii_alphabetic) {
                return Some(self.start_tag());
            }
        }

        // Text runs to the next '<'. A '<' that does not open a tag is literal text.
        let skip = usize::from(b[0] == b'<');
        let end = rest[skip..].find('<').map_or(rest.len(), |i| i + skip);
        self.pos += end;
        Some(Token::Text(decode_entities(&rest[..end])))
    }
}

fn lowercase(s: &str) -> Cow<'_, str> {
    if s.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(s.to_ascii_lowercase())
    } else {
        Cow::Borrowed(s)
    }
}

/// Position of `</tag` (case-insensitive) in `haystack`.
fn find_end_tag(haystack: &str, tag: &str) -> Option<usize> {
    let (h, t) = (haystack.as_bytes(), tag.as_bytes());
    let mut from = 0;
    while let Some(i) = haystack[from..].find("</").map(|i| i + from) {
        let name = i + 2;
        if h.len() >= name + t.len() && h[name..name + t.len()].eq_ignore_ascii_case(t) {
            return Some(i);
        }
        from = i + 2;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_mixed_input() {
        let tokens: Vec<Token> =
            Tokenizer::new("<!doctype html><!-- c --><P Id=a>x &lt; y</p> 1 < 2").collect();
        assert_eq!(
            tokens,
            vec![
                Token::Doctype("doctype html".into()),
                Token::Comment(" c ".into()),
                Token::StartTag {
                    tag: "p".into(),
                    attrs: vec![("id".into(), "a".into())],
                    self_closing: false
                },
                Token::Text("x < y".into()),
                Token::EndTag("p".into()),
                Token::Text(" 1 ".into()),
                Token::Text("< 2".into()),
            ]
        );
    }

    #[test]
    fn borrows_when_nothing_changes() {
        let mut tokens = Tokenizer::new("<div class=row>plain text</div>");
        let Some(Token::StartTag { tag, attrs, .. }) = tokens.next() else {
            panic!("expected a start tag")
        };
        assert!(matches!(tag, Cow::Borrowed("div")));
        assert!(matches!(attrs[0].1, Cow::Borrowed("row")));
        assert!(matches!(
            tokens.next(),
            Some(Token::Text(Cow::Borrowed("plain text")))
        ));
    }

    #[test]
    fn finds_raw_text_end_case_insensitively() {
        let tokens: Vec<Token> = Tokenizer::new("<script>a</b></SCRIPT>").collect();
        assert_eq!(tokens[1], Token::Text("a</b>".into()));
        assert_eq!(tokens[2], Token::EndTag("script".into()));
    }

    #[test]
    fn survives_truncated_input() {
        for input in [
            "<div",
            "<div class=\"x",
            "</di",
            "<!-- open",
            "<!doc",
            "<script>never closed",
            "<script></scr",
        ] {
            let _: Vec<Token> = Tokenizer::new(input).collect();
        }
    }
}
