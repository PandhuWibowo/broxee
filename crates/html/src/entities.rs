use std::borrow::Cow;

/// Decode numeric references and the most common named entities.
/// Unknown entities are left as-is, matching browser behaviour.
/// Borrows the input when there is nothing to decode, which is the common case.
pub fn decode_entities(s: &str) -> Cow<'_, str> {
    if !s.contains('&') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        if let Some(semi) = rest.bytes().take(32).position(|b| b == b';') {
            if let Some(c) = lookup(&rest[1..semi]) {
                out.push(c);
                rest = &rest[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &rest[1..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

fn lookup(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return Some(char::from_u32(code).unwrap_or('\u{FFFD}'));
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "bull" => '•',
        "middot" => '·',
        "times" => '×',
        "deg" => '°',
        "euro" => '€',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::decode_entities;

    #[test]
    fn decodes_named_and_numeric() {
        assert_eq!(
            decode_entities("a &amp; b &#65;&#x42; &copy;"),
            "a & b AB ©"
        );
    }

    #[test]
    fn leaves_unknown_and_bare_ampersands() {
        assert_eq!(decode_entities("Q&A &unknown; &"), "Q&A &unknown; &");
    }
}
