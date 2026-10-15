//! plx_data: the data layer of the application core.
//!
//! `local` (the library: folders of videos on this machine's disk), `local_meta` (the `.nfo`,
//! backdrop and logo beside a title), `local_progress` (where each
//! file was left), `metadata` (the item that is playing) and `stores` (one command vocabulary
//! and one machine per data store). It is the `data` layer of `ci/module-layers.ini`: it uses
//! `base`, `machine` and `platform` and nothing above them, so it names no screen, widget or
//! application type.

pub mod library; // where the library reads from: PITV_MEDIA_DIRS, else library.conf's folders and SMB shares
pub mod local; // the library: folders of videos on this machine's disk, listed off the frame
pub mod local_meta; // what a title says about itself: its Kodi-style .nfo, backdrop and logo beside it
pub mod local_progress; // where each local file was left and whether it was finished, kept on disk
pub mod metadata; // the playing item: its tracks, chapters and the language matching its menus use
pub mod stores; // stores as machines: one command vocabulary + one step per data store
pub mod settings; // the install-wide player settings: subtitle tone, size, placement and the skip interval
