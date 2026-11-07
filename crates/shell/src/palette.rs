//! The ⌘K palette's contents: what can be found (tabs, history, commands, a typed address) and
//! how typed text ranks it. Drawing is in `chrome.rs`, input handling in `browser.rs`.

/// Everything the palette can run. Each one also has a shortcut, listed next to it, so the
/// palette teaches the keyboard (DESIGN.md, section 13).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    NewTab,
    CloseTab,
    ReopenTab,
    SwitchProfile,
    ToggleSidebar,
    FocusAddress,
    ClearCookies,
    Back,
    Forward,
    Reload,
}

impl Command {
    pub const ALL: [Command; 10] = [
        Command::NewTab,
        Command::CloseTab,
        Command::ReopenTab,
        Command::SwitchProfile,
        Command::ToggleSidebar,
        Command::FocusAddress,
        Command::ClearCookies,
        Command::Back,
        Command::Forward,
        Command::Reload,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Command::NewTab => "New tab",
            Command::CloseTab => "Close tab",
            Command::ReopenTab => "Reopen closed tab",
            Command::SwitchProfile => "Switch profile",
            Command::ToggleSidebar => "Show or hide the sidebar",
            Command::FocusAddress => "Edit the address",
            Command::ClearCookies => "Clear this profile's cookies",
            Command::Back => "Back",
            Command::Forward => "Forward",
            Command::Reload => "Reload",
        }
    }

    /// The shortcut, with ⌘ ⇧ ⌥ as the modifier symbols the palette draws as icons. Rare
    /// commands that are easy to run by mistake have none.
    pub fn keys(self) -> Option<&'static str> {
        Some(match self {
            Command::NewTab => "⌘T",
            Command::CloseTab => "⌘W",
            Command::ReopenTab => "⌘⇧T",
            Command::SwitchProfile => "⌘⇧P",
            Command::ToggleSidebar => "⌘\\",
            Command::FocusAddress => "⌘L",
            Command::ClearCookies => return None,
            Command::Back => "⌘[",
            Command::Forward => "⌘]",
            Command::Reload => "⌘R",
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Open a typed address in the current tab.
    Open(net::Url),
    /// Show an open tab, in any profile.
    Tab {
        profile: usize,
        index: usize,
    },
    /// Open a page from the history in the current tab.
    Visit(net::Url),
    Run(Command),
}

/// What is shown at the right end of a result.
#[derive(Clone, Debug, PartialEq)]
pub enum Hint {
    Label(String),
    Keys(&'static str),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub title: String,
    pub detail: String,
    pub hint: Hint,
    pub action: Action,
}

/// Keep the items that match `query` and order them best first; equal scores keep their
/// order. An empty query keeps everything as given.
pub fn rank(query: &str, items: Vec<Item>) -> Vec<Item> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return items;
    }
    let mut scored: Vec<(u32, Item)> = items
        .into_iter()
        .filter_map(|item| {
            // The title counts more than the second line (a URL, a profile name).
            let title = score(&query, &item.title).map(|s| s * 2);
            let detail = score(&query, &item.detail);
            let best = title.max(detail)?;
            Some((best, item))
        })
        .collect();
    // Stable, so equal scores keep the order they were given in.
    scored.sort_by_key(|&(score, _)| std::cmp::Reverse(score));
    scored.into_iter().map(|(_, item)| item).collect()
}

/// How well lowercase `query` matches `text`, or `None`. A substring scores higher than
/// letters found apart, a match at the start of a word higher than one inside it, and an
/// earlier match higher than a later one.
fn score(query: &str, text: &str) -> Option<u32> {
    let text = text.to_lowercase();
    if let Some(at) = text.find(query) {
        let word_start = at == 0 || !text[..at].ends_with(char::is_alphanumeric);
        let bonus = if word_start { 300 } else { 0 };
        return Some(1000 + bonus - (at.min(200) as u32));
    }
    // Every letter in order, possibly apart: "rbk" finds "Rust book".
    let mut rest = text.chars();
    let mut gaps = 0u32;
    for q in query.chars().filter(|c| !c.is_whitespace()) {
        let mut skipped = 0;
        loop {
            let c = rest.next()?;
            if c == q {
                break;
            }
            skipped += 1;
        }
        gaps += skipped;
    }
    Some(500u32.saturating_sub(gaps.min(500)).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, detail: &str) -> Item {
        Item {
            title: title.into(),
            detail: detail.into(),
            hint: Hint::Label(String::new()),
            action: Action::Run(Command::NewTab),
        }
    }

    fn titles(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.title.as_str()).collect()
    }

    #[test]
    fn ranks_substrings_word_starts_and_scattered_letters() {
        let items = vec![
            item("Reopen closed tab", ""),
            item("Close tab", ""),
            item("Rust book", "doc.rust-lang.org"),
            item("Trust and safety", "example.com"),
            item("GitHub", "github.com/rust-lang"),
        ];
        assert_eq!(
            titles(&rank("rust", items.clone())),
            ["Rust book", "Trust and safety", "GitHub"],
            "word start, then inside a word, then only in the second line"
        );
        assert_eq!(
            titles(&rank("close", items.clone())),
            ["Close tab", "Reopen closed tab"]
        );
        assert_eq!(
            titles(&rank("rbk", items.clone())),
            ["Rust book"],
            "letters apart"
        );
        assert!(rank("zzz", items.clone()).is_empty());
        assert_eq!(
            rank("  ", items.clone()),
            items,
            "empty query keeps everything"
        );
    }

    #[test]
    fn every_command_has_a_label_and_most_have_a_shortcut() {
        for c in Command::ALL {
            assert!(!c.label().is_empty());
            assert!(c.keys().is_none_or(|k| k.starts_with('⌘')));
        }
    }
}
