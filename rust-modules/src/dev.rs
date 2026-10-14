//! The developer-trigger surface: files under the runtime's `pitv-*` trigger names that
//! switch on a diagnostic for one run.
//!
//! Every read goes through [`plx_base::devtrig`], a base-layer module that is `#[cfg]`-gated on the
//! `devtriggers` feature: in a `--no-default-features` build `devtrig::flag` is `false` and
//! `devtrig::read` is `None` at COMPILE time, so no trigger can be armed. THIS module holds the
//! application-layer triggers. Never open a trigger path directly; read through `devtrig`.

/// **Turn on GStreamer's own logging** — the `gstlog` trigger.
///
/// An empty trigger logs every category at level 3; content overrides the whole `GST_DEBUG` spec,
/// so `appsrc:6,decodebin:5` or `*:4` both work. The log goes to the runtime directory beside the
/// event log. GStreamer reads these variables at `gst_init`, so this runs before anything can
/// create a pipeline.
///
/// **Not free.** A high level on many categories is a lot of formatted I/O and it is not a setting
/// to leave armed while measuring anything about frame pacing.
#[cfg(feature = "devtriggers")]
pub(crate) fn arm_gst_logging() {
    let Some(spec) = plx_base::devtrig::read("gstlog") else { return };
    let spec = if spec.is_empty() { "*:3".to_string() } else { spec };
    let log = plx_base::paths::in_runtime_dir(plx_base::paths::runtime_file::GST);
    // SAFETY: single-threaded here by construction — this runs before SDL init and before any
    // worker is spawned. `set_var` is only unsound against a concurrent reader.
    std::env::set_var("GST_DEBUG", &spec);
    std::env::set_var("GST_DEBUG_FILE", &log);
    std::env::set_var("GST_DEBUG_NO_COLOR", "1");
    plx_base::eventlog::log(&format!("gstlog: GST_DEBUG={spec} -> {}", log.display()));
}
#[cfg(not(feature = "devtriggers"))]
pub(crate) fn arm_gst_logging() {}

plx_base::devtrig::latched_flag! {
    /// The `imgtrace` trigger — trace the poster pipeline (`app::adapters::poster::trace`).
    pub(crate) fn imgtrace_armed = "imgtrace";
}
plx_base::devtrig::latched_flag! {
    /// RAM-only control leg for cache performance comparisons; absent in shipping builds.
    pub(crate) fn imagecache_bypass_armed = "imagecache-bypass";
}
