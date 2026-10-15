# screens/ — the application's owned screens (read before adding or changing one)

Every `Screen` the dispatcher mounts, steps, focuses and draws lives here (crate `plx_screens`):

| module | screen |
|---|---|
| `home.rs` | Home: the newest title, Continue Watching and Recently Added, under the tab bar |
| `local.rs` | a folder of the local library (one folder per page; SMB shares are mounted folders) |
| `local_detail.rs` | one local title's page: artwork, description, how to play it |
| `item_menu.rs` | the hold menu on a card (play from the start, mark watched, ...) |
| `player/` | the player screen, its HUD overlay and its track/chapter panels |
| `settings.rs` | Settings: subtitle style, playback languages, the read-ahead cache, About |
| `tabs.rs` | the tab bar's destinations: Home, the library's folders, then Settings |
| `titles.rs` | a local title in one line (shared helper, not a screen) |
| `registry.rs` | the application bundle: `AppArg`, the screen ids and the one `mount` match |

The library half is `../../ui/src/` — read its [`CLAUDE.md`](../../ui/src/CLAUDE.md) first; its
four rules bind here too.

## The layer rule

A screen may name `plx_ui`, `plx_appkit` (widgets several screens share: the player HUD, the track
menus, the info card), `plx_data`, `plx_media` and this crate's `registry` — never the application
crate (cargo has no edge to it) and never a sibling screen module.

- **Reaching another screen is a message, not a call.** A screen asks for navigation through the
  effects it emits (`NavOp`), never by mounting a sibling.
- **A screen keeps no focus of its own.** The `FocusEngine` in `Input` holds the current focus and
  every group's remembered cursor; there is no per-screen focus ladder.
- No `static mut`: a screen's state is a field of the instance the container owns.

## Adding a screen

A new screen touches its own module, `registry.rs` (the `AppArg` variant, the screen id, the
`mount` arm) and `lib.rs`. A settings-style page is built on `plx_ui::form::FormTable` and
`plx_ui::table_screen::{TableScreen, Header}`: it focuses by the row's `RowKey`, never by table
index, so reordering a form moves no focus.

## A landing must not move ground somebody is looking at

Content arrives asynchronously (folder listings, artwork, progress). Stage a landing rather than
committing under the viewer's cursor; let `cards::Shelf`/`Grid` absorb an index change of the
focused element (`landed`, `landed_shift`); derive a re-entry position from what is restored
(scroll), not from a stored index into content that may have changed.

## Building a card page

A page of media cards is a `plx_ui::cards::Stack` page: implement `CardSource` over a borrowed view
of the data for each section and `StackPage` on the page's content, hold the `Stack` in the screen,
feed it every event (`stack.on(&page, ev, cx, fx)`) and draw through `stack.view(&page)`. Everything
a layout needs is re-exported from `plx_ui::cards`. `home.rs` and `local.rs` are worked examples.

## Verifying a screen change

Run the tests, then look at it: the headless harness in `AGENTS.md` boots the app under xvfb, drives
it through the remote FIFO and writes screenshots. A screen's `name()` is the route word in the
heartbeat line; keep it stable.
