//! The window layer, written against the operating system directly. macOS only for now:
//! Linux (Wayland/X11) and Windows get their own modules behind the same API later.
//!
//! The event types are shared by every platform, so code that reacts to input can be built
//! and tested anywhere.

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub use macos::{release_free_memory, run_app, Window};

/// Input from the user, in window points with the origin at the top left of the content area.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Positive `dy` moves the content down (the user scrolled up); (x, y) is the pointer.
    Scroll {
        dy: f64,
        x: f64,
        y: f64,
    },
    /// The primary button went down.
    Click {
        x: f64,
        y: f64,
    },
    /// The pointer moved with the button down.
    Drag {
        x: f64,
        y: f64,
    },
    /// The button came up.
    Release {
        x: f64,
        y: f64,
    },
    Key(KeyEvent),
    /// Text from the clipboard, read on the main thread when the user pressed Cmd+V.
    Paste(String),
    /// The content area changed size or moved to a display with another pixel density.
    Resize {
        width: f64,
        height: f64,
        scale: f64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyEvent {
    pub key: Key,
    pub command: bool,
    pub shift: bool,
    /// The Option (Alt) key: ⌘⌥B opens the bookmarks panel.
    pub option: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Key {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Tab,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
}
