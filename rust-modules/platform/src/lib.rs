//! plx_platform: the localization catalog (`i18n`) and the artwork cache (`imgcache`).
//!
//! It is the `platform` layer: it uses `base` and `machine` and nothing else. The crate owns its
//! build script, which generates the `i18n::msg` catalog from `locales/en`.
//!
//! `test-support` exposes the `cfg(test)` seams other layers' tests build on and keeps this
//! crate's test-mode behaviour when a dependent's tests build it. The application crate enables it
//! in `[dev-dependencies]` only, so no shipped build sees it.

pub mod i18n;
pub mod imgcache; // bounded persistent artwork cache shared by every image source
