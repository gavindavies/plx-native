//! plx_base: the leaf layer of the PiTV application core.
//!
//! The event log, file locations, the spawn discipline, the dlopen seam, the frame instruments and
//! the small leaf utilities every other layer calls. It names nothing outside itself.
//!
//! `test-support` exposes the `cfg(test)` fixtures other layers' tests build on (`testlock`,
//! `testscratch`, ...) and keeps this crate's test-mode behaviour (a disarmed watchdog, no SDL
//! link in `diag::heartbeat`) when a dependent's tests build it. The application crate enables it
//! in `[dev-dependencies]` only, so no shipped build sees it.

pub mod identity; // the product name and the build's version
pub mod eventlog; // THE event log: `eventlog::log` is the one sink every module writes through
pub mod paths; // where the app's own files live — /proc/self/exe, not a hardcoded install prefix
pub mod task; // the one spawn: a refused thread is a return value, not a panic that kills the app
pub mod cbuf; // fixed NUL-terminated C-string buffer read/write
pub mod sha256; // SHA-256, hand-written: the artwork cache's content keys
pub mod dynlib; // dlopen by absolute path: the bundled FFmpeg and libass
pub mod fontcov; // which codepoints a font file can draw, read from its cmap
pub mod surface; // what we are actually drawing into: drawable vs the 1920x1080 logical canvas
pub mod tile; // `Tile`: the shelf-tile trait the data layer implements and `ui` draws through
pub mod devtrig; // the runtime-dir trigger PRIMITIVES (`flag`, `read`, `latched_flag!`...)
pub mod diag; // the frame instruments (`heartbeat`, `spans`) and the zlib helper
#[cfg(any(test, feature = "test-support"))]
pub mod testlock; // one lock for every test that touches a process-global
#[cfg(any(test, feature = "test-support"))]
pub mod testscratch; // per-process scratch directories a test binary removes when it exits
