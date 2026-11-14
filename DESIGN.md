# Broxee design direction

> Drafted by the agent at the owner's request (2026-11-06), from the owner's brief and the Anveesa family
> direction in `anveesa-official-website/DESIGN.md`. Agent-written direction drifts toward default AI taste,
> so treat every line as editable. The owner's edits win.
>
> Owner's brief: modern, minimal, simple and intuitive, fast, anti AI slop, everything under 60 MB.
> Owner's choices: signature color emerald; antislop applied during the work.
> Agent's choices are marked **(agent)** so they are easy to find and overrule.

Agents treat this file as design data, then apply `antislop` as the filter. If code and this file disagree, fix one
of them.

Dial: ENERGY 1 / RHYTHM 1 / MOTION 1

- ENERGY 1: the web page is the product. Browser chrome is quiet, flat and low-contrast so the page carries the eye.
- RHYTHM 1: one sidebar, one pane header, one menu style, one dialog style. Learned once, used everywhere.
- MOTION 1: hover and press states, a panel that slides in, a palette that fades in. Nothing loops.

Design Read: a desktop web browser for people who keep work and personal accounts open all day, in the calm
Anveesa family language, dial ENERGY 1 / RHYTHM 1 / MOTION 1.

---

## 1. Identity

Broxee is a small, fast browser with its own engine. It keeps work and personal life apart without making you
think about it, and it remembers exactly how you left it.

Personality, inherited from the family: warm, plain-spoken, calm. Broxee never nags, never shows a badge to get
attention, and says honestly when its young engine cannot show a page.

The brief says "seamless". In practice that means five concrete things, and the word itself never appears in the UI:

1. No separate top toolbar. The address field lives in the sidebar, so the page gets the full window height.
2. Closing and reopening Broxee brings back every window, pane, tab group and scroll position.
3. One keyboard palette (⌘K) reaches every tab, bookmark, history entry and command.
4. Switching tab, profile or split layout is instant. No transition waits on the user.
5. A tab that sleeps to save memory wakes when you select it, with its scroll position intact.

## 2. Principles

1. **The page first.** Chrome recedes: neutral surfaces, hairlines, no decoration.
2. **Always know which account you are in.** Every pane shows its profile by name and color. Never by color alone.
3. **Keyboard first, mouse friendly.** Every action has a shortcut listed in ⌘K, and every control is reachable
   with Tab.
4. **Fast is a design rule.** No animation longer than 120 ms, none that blocks input, and no screen that waits
   for an animation to finish.
5. **Small is a design rule.** The whole app stays under 60 MB (see section 10). A design idea that needs a large
   asset is redesigned.
6. **Honest about the engine.** When Broxee cannot render something yet, it says so and offers a way out.

## 3. Color

Sky and navy from the family, plus emerald as Broxee's signature. Every pair below was checked for WCAG AA with
the antislop contrast checker.

| Token | Light | Dark | Use |
|---|---|---|---|
| `canvas` | `#eef4fa` | `#070d18` | Sidebar and window background |
| `surface` | `#ffffff` | `#0f1a2c` | Pane headers, menus, the ⌘K palette, Broxee's own pages |
| `selected` | `#dcebf7` | `#16233a` | The active tab row and the highlighted ⌘K result |
| `line` | `#d3dfea` | `#1c2a40` | Hairlines between sidebar, panes and headers |
| `text` | `#0d1b2a` | `#e6eef8` | Tab titles, menu items, body text |
| `text-2` | `#4b5d71` | `#a3b4c9` | Hosts in the pane header, secondary labels |
| `muted` | `#5f7186` | `#8a9cb4` | Sleeping tab titles, shortcut hints, timestamps |
| `accent` | `#0369a1` | `#38bdf8` | Links, the primary button, focus ring, load progress |
| `on-accent` | `#ffffff` | `#04233a` | Text on an `accent` fill |
| `signature` | `#047857` | `#34d399` | The Broxee wordmark and the default Personal profile |

Measured contrast (AA needs 4.5:1 for normal text):

| Pair | Light | Dark |
|---|---|---|
| `text` on `canvas` / `surface` | 15.7 | 14.9 |
| `text` on `selected` | 14.3 | 13.4 |
| `text-2` on `canvas` / `surface` | 6.1 | 8.2 |
| `text-2` on `selected` | 5.6 | 7.4 |
| `muted` on `canvas` / `surface` | 4.52 | 6.2 |
| `accent` on `canvas` / `surface` | 5.4 | 8.1 |
| `on-accent` on `accent` | 5.9 | 7.5 |
| `signature` on `canvas` / `surface` | 4.95 | 9.1 |

Rules:

- **`muted` is at the limit in light mode (4.52).** Never lighten it, and never put it on `selected`. A sleeping tab
  that gets selected wakes up, so its title returns to `text` anyway.
- **Marks over page text are translucent `accent`** (selection 40%, find matches 24%, the current match 51%),
  never an opaque fill: a page's own colors and text stay readable under them, in either theme. The find
  field takes the address field's place in the pane header, with "Find in page" as its placeholder and
  "2 of 5" (or "No matches") muted at its right end.
- **One accent.** Sky marks the thing you can act on now: a link, the focused control, the primary button, page
  load progress. It is never a background for a whole region.
- **Emerald is a signature, not a second accent.** It appears on the wordmark and as a profile color. Buttons and
  focus are always sky, following the family rule.
- No gradients, glows, glass or background patterns anywhere in the chrome.
- Theme follows the OS until the person picks Light or Dark in Settings. Both themes are first-class.

### Profile colors

Profiles are the reason Broxee exists, so their color is a real state marker, not decoration.

| Profile hue | Light | Dark | Contrast light / dark |
|---|---|---|---|
| Emerald (default for **Personal**) | `#047857` | `#34d399` | 4.95 / 9.1 |
| Amber (default for **Work**) **(agent)** | `#b45309` | `#fbbf24` | 4.53 / 10.4 |
| Rose **(agent)** | `#be123c` | `#fb7185` | 5.67 / 7.23 |
| Violet **(agent)** | `#6d28d9` | `#a78bfa` | 6.41 / 7.15 |

More hues can be added for extra profiles, each checked to 4.5:1 on `canvas` in both themes before it ships. New
profiles take the hue fewest profiles use. Sky is left out on purpose (it is the `accent`), and greys too:
next to `text-2` and `muted` they would not read as a profile color.
Amber in light mode is at the limit (4.53): never lighten it.

## 4. Typography

- **UI: Figtree**, the family's body face, bundled with the app so tabs and menus render identically on every OS.
  Its open, friendly shapes stay legible at the small sizes a sidebar needs. Two static faces, Regular (400) and
  SemiBold (600), stored in the binary as WOFF2: 46 KB together, decoded the first time the interface draws text.
  A variable font would need glyph variations (`gvar`) Broxee does not read yet; 500 renders as 400.
- **Modifier symbols (⌘ ⇧ ⌥)** are not in Figtree. They come from the system's symbol font (macOS
  `Keyboard.ttf`) through per-character fallback, and are drawn as icons where no font has them.
- **Wordmark: Figtree semibold** in `signature`. No separate display face: Broxee has no headlines in its chrome,
  so a second font would cost bytes for nothing. **(agent)**
- **No monospace in the chrome.** URLs are shown in Figtree. A mono face arrives only if developer tools do.

| Role | Size / weight | Example |
|---|---|---|
| Tab title | 13px / 500 | "Pull requests · anveesa-cli" |
| Pane header host | 12.5px / 500, `text-2` | "github.com" |
| Section label in sidebar | 12px / 600, `muted` | "Pinned", "Research" (a tab group) |
| ⌘K result | 13.5px / 500, secondary line 12px `text-2` | "Bookmarks: Rust book" |
| Broxee page title (Settings, error pages) | 20px / 650 | "This page needs JavaScript" |
| Body on Broxee pages | 14px / 400 | Explanations, settings descriptions |

Sentence case everywhere. No uppercase tracked labels. Numbers use tabular figures.

## 5. Shape and depth

- Radius: 6px for tab rows, inputs and buttons; 10px for menus, the ⌘K palette and dialogs. Nothing is pill-shaped.
- Shadow: only on things that float above the page (menus, ⌘K, dialogs, a select's dropdown), one soft shadow.
  A select's dropdown is a menu panel under its control (above it when there is no room), at least 160px
  wide, ten rows before it scrolls: group labels as section labels, disabled options in `muted`, the current
  choice in semibold (not marked by color alone), the highlighted row on `selected`. Everything else is flat
  and separated by `line` hairlines.
- Panes in a split are separated by a 1px `line` hairline and a 6px drag handle (`text-2`) that only shows on
  hover. A tab shown in a pane carries a 16px outlined box with the pane's number (`text-2`) in its sidebar
  row: a number, never only a color. A tab dragged over a pane washes it in translucent `accent`.

## 6. Identity motif: the profile edge **(agent)**

Every pane carries a **2px line in its profile's color along its top edge**, directly above the pane header, which
names the profile in text ("Work", "Personal").

- In a single pane it is a quiet reminder of which account you are in.
- In a split of two or four panes, each pane has its own edge, so a Work Slack next to a Personal WhatsApp is
  obvious at a glance.
- The profile switcher at the top of the sidebar uses the same 2px edge, so the motif ties the sidebar to the panes.

It is the one place Broxee uses color as a stripe, and it marks a real state (which cookies and logins this pane
uses). It never appears as decoration anywhere else.

## 7. Layout

```
┌────────────────────┬──────────────────────────────────────────────┐
│ ▬▬ Work        ⌄   │▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬│ ← profile edge
│ Search or go to ⌘K │ ‹ › ↻  github.com · Work               ⋯     │ ← pane header
│                    │──────────────────────────────────────────────│
│ Pinned             │                                              │
│   Mail             │                                              │
│   Calendar         │              the web page                    │
│ Research         ⌄ │                                              │
│   Rust book        │                                              │
│   Servo docs       │                                              │
│ Today              │                                              │
│   Pull requests  × │                                              │
│   Example Domain   │                                              │
│                    │                                              │
│ + New tab          │                                              │
└────────────────────┴──────────────────────────────────────────────┘
```

- **Sidebar (left, 240px, resizable 200 to 320px, hidden with ⌘\\):** profile switcher, the address and search
  field, pinned tabs, tab groups, today's tabs, "New tab". Each profile has its own tabs and groups.
- **Pane header (one per pane, 36px):** back, forward, reload, the host and the profile name, and a ⋯ menu. The full
  URL appears when the field is focused (⌘L). No other toolbar.
- **Split view:** ⌘⇧1 single pane, ⌘⇧2 two side by side, ⌘⇧4 a 2×2 grid. Drag a tab onto a pane to put it there.
  Panes can use different profiles.
- **New tab:** the ⌘K palette, focused, over a blank `surface`. It lists real recent tabs and bookmarks. No speed
  dial tiles, no suggestions Broxee did not get from your own history.
- **Bookmarks:** ⌘D saves the current page to a folder (last used folder preselected). ⌘⌥B opens the bookmarks
  panel in place of the tab list. Every bookmark is also a ⌘K result, so most people never open the panel.
- **Settings:** a Broxee page in a tab, not a modal: Profiles, Appearance, Privacy, Sleeping tabs, Shortcuts.

### Shortcuts

| Action | Keys |
|---|---|
| Palette: tabs, bookmarks, history, commands | ⌘K |
| Focus the address field | ⌘L |
| New tab / close tab / reopen closed tab | ⌘T / ⌘W / ⌘⇧T |
| Go to tab 1 to 9 | ⌘1 … ⌘9 |
| Single / two panes / four panes | ⌘⇧1 / ⌘⇧2 / ⌘⇧4 |
| Show or hide the sidebar | ⌘\\ |
| Bookmark this page / bookmarks panel | ⌘D / ⌘⌥B |
| Switch profile | ⌘⇧P |
| Find in page / next / previous match | ⌘F, then Enter / Shift+Enter (or ⌘G / ⌘⇧G); Esc closes |
| Select all page text / copy the selection | ⌘A / ⌘C |

## 8. Icons

A small set drawn by us as vector paths in code (back, forward, reload, close, more, split, bookmark, lock,
expand), 16px on a 1.5px stroke with square ends **(agent)**. Drawing them in code costs a few kilobytes instead of
an icon font, and keeps them from looking like a stock library. Everything else is a text label. No emoji, no
sparkles, no decorative icons.

Favicons come from the site. When a site has none, the tab shows the first letter of its host on `selected`.

## 9. States

Every list and every page Broxee draws has its empty, loading and error state.

| Situation | What Broxee shows |
|---|---|
| Page loading | A 2px `accent` progress line under the pane header, only while loading. Reload becomes Stop. |
| Sleeping tab | Title in `muted`. Tooltip: "Asleep to save memory. Select to wake." |
| Network error | Broxee page: what failed in plain words ("Couldn't reach github.com: the connection was reset") and **Try again**. |
| Certificate error | "github.com's certificate isn't valid, so Broxee didn't open it." No click-through in the first version. |
| Page Broxee can't render yet | "This page needs JavaScript, which Broxee doesn't run yet." Action: **Open in default browser**. |
| No bookmarks | "No bookmarks yet. Press ⌘D on any page to save it here." |
| ⌘K finds nothing | "Nothing matches "rust bok". Press Enter to search the web." |
| Restored session | Tabs come back asleep except the visible ones, so startup stays fast. No banner announcing it. |

## 10. Size and speed budget for the design

The design has to fit inside the 60 MB budget enforced by `scripts/budget.sh`.

| Item | Budget |
|---|---|
| Figtree (Regular + SemiBold, WOFF2) | under 1 MB (46 KB) |
| Icons (vector paths in code) | under 20 KB |
| Images in the chrome | none |
| Animation duration | 120 ms maximum, ease-out; off under reduced motion |
| Input to visible response (tab switch, split change, ⌘K open) | one frame target, 50 ms ceiling |

## 11. Logo

**[LOGO]** No mark has been designed. Until the owner decides one, the wordmark "Broxee" in Figtree semibold,
`signature` color, stands in for it, and the app icon is a placeholder that says so in its file name.

## 12. Voice

Plain verbs that say what happens: "Try again", "Open in default browser", "Move to Work", "Reopen closed tab".
Empty states say why they are empty and the one action that fills them. No emoji, no em dashes, no buzzwords
(including "seamless", which became the concrete list in section 1).

## 13. Accessibility

- Focus ring: 2px `accent` with a 2px offset on every focusable control, in both themes.
- Every action works from the keyboard; ⌘K lists every shortcut.
- Profiles are always named in text next to their color.
- Minimum hit area 28px in the sidebar (desktop pointer), 44px for anything that may be touched on a touchscreen
  laptop.
- Reduced motion turns every transition off.
