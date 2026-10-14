//! The `/tmp` developer-trigger PRIMITIVES: the one door onto `/tmp/pitv-*`.
//!
//! This app is driven headlessly by ~44 files under `/tmp/pitv-*`: which screen to boot to,
//! which item to play, which URL to stream, whether to auto-press OK, which PMS token to use.
//! That is how `tests/run.py` and every capture scene work, and it is not going away. This module
//! is the READ side of that surface — [`flag`], [`read`], [`read_sample`], the per-process latch
//! [`latched_flag!`], and the handful of triggers whose answer is a plain value ([`no_wan`],
//! [`holdload_delay_ms`], [`guard_log_only`]). It names no application type, so every layer can
//! call it. What a trigger DOES once read — the arms that boot a screen, press a key, feed the
//! player a URL — is `dev` and its `scenarios`, which names the app and sits above
//! everything; a typed trigger (a value that needs an upper layer's type to parse into) lives next
//! to the one lower-layer module that consumes it, parsed from [`read`].
//!
//! It must not exist in a public build. `/tmp` is the SHARED system `/tmp` in the production jail
//! too (mode 1777, both jail profiles), so on an ordinary user's TV every one of those files is a
//! behaviour switch any co-resident process can throw. Three are outright takeovers:
//! `pitv-token` beats the signed-in session (`app.rs`'s boot gate), `pitv-servers` hands
//! the app a whole additional server — an address AND the token to trust it with — and
//! `pitv-url` replaces the stream the player feeds.
//!
//! So every read goes through here, and here is `#[cfg]`-gated on the `devtriggers` feature. In a
//! `--no-default-features` build [`flag`] is `false` and [`read`] is `None` at COMPILE time, so
//! no trigger can be armed. Storage and diagnostics still use runtime files; none are developer
//! triggers.
//!
//! That does NOT keep a trigger's NAME out of the binary. A branch behind `flag`/`read` usually
//! folds away, but not reliably: once the answer is carried through a struct field (the
//! `nobudget` flag on `DevFlags`), the optimizer may keep the branch and its string literals
//! in a release build, and `ci/check-package.py` fails the package because it greps the shipped
//! bytes for every trigger name `dev`'s `DIAG` list and [`CONTROLLED`] declare. So every
//! statement whose literal names a trigger (a log line saying `/tmp/pitv-…`) carries its own
//! `#[cfg(feature = "devtriggers")]`. Never rely on constant folding for this.
//!
//! **Never open a `/tmp` path directly.** The grep that audits this (`/tmp/pitv-` outside
//! `dev` and the unconditional log sinks, `ci/check-deps.sh`'s `tmppath`) is the only thing
//! keeping the property true. A trigger's bare name is the argument to [`flag`] / [`read`] and the
//! `pitv-` prefix is added in [`path`], so no literal path exists to find.

/// The triggers a CONTROLLED boot (`app::bootstrap`: the recorder, a replay, an explicit
/// `app-init`) may carry, as bare names. Anything else armed on a recording boot makes its typed
/// initial unsupported, so a replay cannot silently run under a trigger it never modelled.
///
/// One gated table rather than literals at each consumer, for the reason `dev`'s `DIAG` is
/// gated: a full trigger name in the release binary is exactly what `ci/check-package.py` grades as
/// "dev triggers compiled in", and `pitv-noidle` is its witness. A release build never arms a
/// trigger (`dev::armed_triggers` is empty there), so it has no vocabulary to check against
/// and the accessors below answer "not supported" / "not listed" without naming one.
///
/// `ci/check-package.py` parses this array out of THIS file (and `DIAG` out of `dev.rs`), so it
/// stays one array literal of bare names, and no other text in this file spells its declaration.
#[cfg(any(feature = "devtriggers", test, feature = "test-support"))]
const CONTROLLED: &[&str] = &[
    "rec", "recplay", "focus", "noidle", "token", "app-init", "settings",
    "detail", "detailsec", "detailok", "filmography", "personcredits", "nowan",
];

/// Is the recorded trigger `trigger` (full `pitv-<name>` form, as `dev::armed_triggers`
/// lists it) one a controlled boot supports? See [`CONTROLLED`].
#[cfg(any(feature = "devtriggers", test, feature = "test-support"))]
pub fn controlled_trigger(trigger: &str) -> bool {
    trigger.strip_prefix("pitv-").is_some_and(|name| CONTROLLED.contains(&name))
}
#[cfg(not(any(feature = "devtriggers", test, feature = "test-support")))]
pub fn controlled_trigger(_trigger: &str) -> bool {
    false
}

/// Does a recorded trigger list (full names, as `dev::armed_triggers` returns them) carry
/// the trigger `name` (bare)? The typed-initial counterpart of [`flag`]: it reads the list a boot
/// was captured with, never the filesystem. Always `false` in a release build, whose list is empty.
#[cfg(any(feature = "devtriggers", test, feature = "test-support"))]
pub fn listed(triggers: &[String], name: &str) -> bool {
    triggers.iter().any(|trigger| trigger.strip_prefix("pitv-") == Some(name))
}
#[cfg(not(any(feature = "devtriggers", test, feature = "test-support")))]
pub fn listed(_triggers: &[String], _name: &str) -> bool {
    false
}

/// Is the trigger `name` (bare, without the `pitv-` prefix) present?
#[cfg(feature = "devtriggers")]
pub fn flag(name: &str) -> bool {
    path(name).exists()
}
#[cfg(not(feature = "devtriggers"))]
pub fn flag(_name: &str) -> bool {
    false
}

/// [`flag`], answered ONCE for the whole process.
///
/// **A `devtrig::flag` is a `stat`, so a trigger read every frame is a syscall on the 60 fps path.**
/// Latching also fixes a correctness wrinkle that has nothing to do with cost: `tests/run.py`
/// clears `/tmp/pitv-*` between cases, so a later read can legitimately find the file gone
/// mid-run and a per-frame probe would change its answer half way through a case.
///
/// A macro rather than a function because the latch has to be a `static` per trigger, and a
/// function would need a map behind a lock — which is the thing being avoided. It lives HERE
/// because this module is the one door onto the `/tmp` surface; it was briefly a file-local macro
/// in `ui/widgets.rs`, which walled it off from the other per-frame `flag` callers
/// (`focusprobe::armed` had already hand-rolled exactly this body, doc comment and all).
///
/// No `#[cfg]` arms, deliberately: [`flag`] is already `false` at COMPILE time without the
/// `devtriggers` feature, so a second gate here would only re-derive what the door behind it
/// guarantees.
///
/// ```ignore
/// crate::devtrig::latched_flag!(
///     /// `/tmp/pitv-flattabs` — the material off, for an A/B against the flat capsule.
///     fn flat_tabs_armed = "flattabs";
/// );
/// ```
#[macro_export]
#[doc(hidden)] // reached as `devtrig::latched_flag!` through the re-export below
macro_rules! __latched_flag {
    ($(#[$m:meta])* $vis:vis fn $name:ident = $trigger:literal;) => {
        $(#[$m])*
        $vis fn $name() -> bool {
            static SEEN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            *SEEN.get_or_init(|| $crate::devtrig::flag($trigger))
        }
    };
}
pub use __latched_flag as latched_flag;

/// The trigger's CONTENT, trimmed. `Some("")` for a trigger armed as an empty file — several
/// distinguish empty (take the default) from a value (`autoseek`, `library`, `marker`), so an
/// empty file must not read the same as an absent one.
#[cfg(feature = "devtriggers")]
pub fn read(name: &str) -> Option<String> {
    std::fs::read_to_string(path(name))
        .ok()
        .map(|s| s.trim().to_string())
}
#[cfg(not(feature = "devtriggers"))]
pub fn read(_name: &str) -> Option<String> {
    None
}

/// Boot-latched main-thread checker escape hatch. File content must be exactly `log`.
#[cfg(feature = "threadcheck")]
pub fn guard_log_only() -> bool {
    static MODE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| read("guard").as_deref() == Some("log"))
}

/// **Hold the Load-returned flag** — `pitv-holdload[=ms]`.
///
/// `Some(ms)` when armed (default 30000 for a bare/empty trigger), else `None`.
/// `player::threads::load_thread` sleeps this many milliseconds right after the sink's Load returns
/// and BEFORE that fact is published, so the pump's Load budget can be exercised on demand.
#[cfg(feature = "devtriggers")]
pub fn holdload_delay_ms() -> Option<u64> {
    let raw = read("holdload")?;
    Some(if raw.is_empty() {
        30_000
    } else {
        raw.parse().unwrap_or(30_000)
    })
}
#[cfg(not(feature = "devtriggers"))]
pub fn holdload_delay_ms() -> Option<u64> {
    None
}

/// `true` when this build reads the runtime-dir triggers at all — for the one boot log line that
/// says so, and for call sites gating a whole subsystem (the remote FIFO) rather than a read.
pub const ENABLED: bool = cfg!(feature = "devtriggers");

/// The trigger's absolute path. `/tmp/pitv-<name>` on the television; see
/// [`crate::paths::runtime_dir`] for why a host build may put the whole namespace elsewhere.
///
/// `test` is in the cfg beside the feature, and only for a compile reason: the test below writes
/// through this door rather than through a literal, and it guards itself at RUNTIME on
/// [`ENABLED`] — but a runtime guard cannot stop a call from being compiled, so without this the
/// whole crate failed to build under `--no-default-features --test` (E0425, "cannot find function
/// `path` in module `super`"). A shipping release build is unchanged: `cfg(test)` is false there,
/// and the fn is gone exactly as before.
///
/// `pub(crate)` so `dev::scenarios`, which hands a trigger's PATH (not its content) to the
/// recorder's own parser, goes through the same door instead of re-spelling the prefix.
#[cfg(any(feature = "devtriggers", test, feature = "test-support"))]
pub fn path(name: &str) -> std::path::PathBuf {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(root) = private::root() {
        return root.join(format!("pitv-{name}"));
    }
    crate::paths::in_runtime_dir(&format!("pitv-{name}"))
}

/// Run `f` with THIS thread's trigger namespace redirected to a fresh private directory, which is
/// removed afterwards (also when `f` panics).
///
/// **A host test that arms a trigger must not do it in the shared runtime root.** A host test run
/// resolves that root to the bare `/tmp` (only the `hostsim` feature lets `PITV_RUNTIME_DIR`
/// steer it, [`crate::paths::ENV_STEERABLE`]), and the root is one directory for the whole
/// machine: every test binary of this `make check`, and of any other checkout's, reads the same
/// `/tmp/pitv-*`. `jail_fixture_keeps_repair_open_through_the_screen_tick` armed
/// `pitv-failtest` there for a few milliseconds, and a second process's
/// `a_click_on_the_picture_toggles_play_pause_but_not_over_a_failure` that read the file in that
/// window saw a failed playback and swallowed its click (`left: []  right: [Transport(None)]`;
/// 119 of 150 runs with the two side by side). `serial()` cannot help, it is one process's lock.
///
/// Only the calling thread is redirected, so a test that spawns a worker which reads a trigger
/// must keep arming it in the shared root (none does today). Everything resolves through
/// [`path`], so [`flag`] and [`read`] see the private root; [`read_sample`] and the log sinks
/// do not. Not nestable: the inner call replaces the outer root for its own extent.
#[cfg(any(test, feature = "test-support"))]
pub fn with_private_triggers<R>(f: impl FnOnce() -> R) -> R {
    private::with(f)
}

#[cfg(any(test, feature = "test-support"))]
mod private {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    thread_local! {
        static ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    pub(super) fn root() -> Option<PathBuf> {
        ROOT.with(|r| r.borrow().clone())
    }

    pub(super) fn with<R>(f: impl FnOnce() -> R) -> R {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "plx-private-triggers-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("a private trigger directory");
        struct Restore(Option<PathBuf>, PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                ROOT.with(|r| *r.borrow_mut() = self.0.take());
                let _ = std::fs::remove_dir_all(&self.1);
            }
        }
        let _restore = Restore(ROOT.with(|r| r.borrow_mut().replace(dir.clone())), dir);
        f()
    }
}

#[cfg(test)]
mod tests {
    /// An empty trigger file and an absent one mean different things to several call sites
    /// (`autoseek` empty = one seek to 140s; `navosc` empty = Home <-> the first library section).
    #[test]
    fn empty_trigger_is_some_not_none() {
        if !super::ENABLED {
            return; // a release build reads nothing; nothing to distinguish
        }
        // Armed in a private root. It used to write `devtest-empty` into the shared runtime root
        // (the bare `/tmp` on a host test run), where a second checkout's copy of this test raced
        // the same file, and where `dev`'s `a_directory_is_not_an_armed_trigger` scans.
        // Write through `path()` itself, NOT a literal and NOT `env::temp_dir()`: the write and the
        // read must go through the one door.
        let got = super::with_private_triggers(|| {
            let p = super::path("devtest-empty");
            std::fs::write(&p, "").unwrap();
            super::read("devtest-empty")
        });
        assert_eq!(
            got.as_deref(),
            Some(""),
            "an empty trigger must not read as absent"
        );
    }

    /// A private trigger root is private: the write lands outside the shared runtime root, is
    /// visible to `flag`/`read` on this thread, invisible to another thread, and gone afterwards.
    #[test]
    fn private_triggers_stay_out_of_the_shared_runtime_root() {
        if !super::ENABLED {
            return;
        }
        let name = "devtest-private";
        let shared = super::path(name);
        let (inside, armed, other_thread) = super::with_private_triggers(|| {
            let p = super::path(name);
            std::fs::write(&p, "x").unwrap();
            let other = std::thread::spawn(move || super::flag(name)).join().unwrap();
            (super::flag(name), p, other)
        });
        assert!(inside, "the arming thread sees its own trigger");
        assert!(!other_thread, "another thread keeps the shared root");
        assert_ne!(armed, shared);
        assert_ne!(
            armed.parent(),
            shared.parent(),
            "{armed:?} is directly in the shared runtime root"
        );
        assert!(!armed.exists(), "the private root is removed afterwards");
        assert_eq!(super::path(name), shared, "the redirect ends with the closure");
    }

    /// The redirect and the removal also end when the body panics.
    #[test]
    fn a_panicking_body_still_restores_the_root() {
        let shared = super::path("devtest-panic");
        let result = std::panic::catch_unwind(|| {
            super::with_private_triggers(|| panic!("body"));
        });
        assert!(result.is_err());
        assert_eq!(super::path("devtest-panic"), shared);
    }
}
