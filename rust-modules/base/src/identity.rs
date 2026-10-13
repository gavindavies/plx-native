//! **What this program calls itself**: the product name and the build's version, for every
//! surface that reports either (the failure read-out's support line, the event log's banner).
//!
//! The version is handed in by the application, because a `cargo:rustc-env` reaches one crate only
//! and the rule that derives it (`rust-modules/build.rs`) is written exactly once: the application
//! passes `env!("PLX_VERSION")` to [`set_version`] as its first act.

/// The product name.
pub const PRODUCT: &str = "PiTV";

static VERSION: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

/// Record the application's version. Idempotent for the same string; a second, different one is a
/// wiring bug and is refused loudly in a debug build rather than silently re-labelling the build.
pub fn set_version(version: &'static str) {
    let kept = VERSION.get_or_init(|| version);
    debug_assert_eq!(*kept, version, "identity::set_version called with two versions");
}

/// The build's version, or `"unknown"` before [`set_version`] (a layer's own tests).
pub fn version() -> &'static str {
    VERSION.get().copied().unwrap_or("unknown")
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_version_is_what_the_application_set() {
        super::set_version("1.2.3");
        assert_eq!(super::version(), "1.2.3");
        super::set_version("1.2.3");
    }
}
