//! **The application's OWNED screens** (restructure spec §2.1 `screens/`, phase 5b): the Settings
//! family — the surface with its own stack, the root, Legal and its documents, Privacy & data
//! (consent, in both its modes) and Favourite libraries (onboard, in both its modes) — as
//! `Screen` impls the dispatcher mounts, steps, focuses through the engine, hit-tests through the
//! map and draws. No `static mut` here: every screen's state is a field of the instance the
//! container owns (§6.1), and the family's shared visual grammar is `ui/table_screen.rs`'s
//! components (5a) plus `ui/route_screen.rs`'s layout, which stays a library concern.
//!
//! Layer rule (§2.1): a screen names `ui/`, `stores/`, the data crates and this directory's
//! `registry`; never `app/` and never a sibling screen module — a rule `ci/check-deps.sh`'s
//! `layer` and `sibling` gates hold from phase 10. The `Arg` enum, the page alphabet and the
//! mounter's one match are `registry`'s since that phase; what is left in `app/bridge.rs` is the
//! concrete host and the rig it lends the dispatcher.

pub mod local; // the Linux build's folder browser: one folder of the local library per page
pub mod local_detail; // one local video's title page: its artwork, description and how to play it
pub mod home; // Home: the newest title, Continue Watching and Recently Added, under the tab bar
pub mod titles; // a local title in one line: its facts, a bare file's, its watch mark (shared, not a screen)
pub mod tabs; // the tab bar's destinations: Home, the library's folders, then Settings
pub mod settings; // Settings: subtitle style, playback, cache and About, the tab bar's last pill
pub mod item_menu;
pub mod player;
pub mod registry;
