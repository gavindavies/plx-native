//! Which webOS this television actually is, the cached playback capabilities it reports
//! ([`caps`]), and the request which hands the screen back ([`go_home`]).
//!
//! # Why the app needs to know, when it never did before
//!
//! Until now the only thing it asked was "does `libAcbAPI` exist" (`starfish.c`'s `vp_mode`), which
//! splits the world at exactly webOS 5.0 and nowhere else. That was enough while 4.x was the only
//! target. It is not enough now: Kodi — which plays video on 5, 6 and 10 — carries gates at
//! `>= 6` (audio re-setup) and `< 11` / `>= 11` (a seek fallback, a changed signature), and with a
//! single boolean those are literally inexpressible here.
//!
//! The more immediate value is smaller and worth more: **a bug report from hardware nobody here
//! owns currently cannot say which firmware it came from.** The webOS 6/10 playback failure was
//! reported as one thing and is probably two, and the logs could not tell them apart.
//!
//! # Where it comes from
//!
//! `/var/run/nyx/os_info.json`, read once, at boot. Kodi asks
//! `luna://com.webos.service.config/getConfigs` for `tv.nyx.platformCode`; this is the same
//! information from the file nyx writes, and it needs no LS2 client, no subscription and no
//! thread. Verified present on the dev set (webOS 4.5), which reports:
//!
//! ```text
//! "webos_release": "4.10.2",  "webos_release_codename": "goldilocks2-grampians"
//! ```
//!
//! The CODENAME is the more useful half and is why this reads the file rather than a version
//! service: webosbrew's own compatibility data buckets firmware by codename, one library set per
//! bucket (their `library-version` guide — `goldilocks` is 4.0~4.4, `goldilocks2` 4.5~4.10). So
//! logging it says which of THEIR buckets a report belongs to, not just a number.
//!
//! Parsed by hand because this is a flat object of string values and a parser that cannot fail is
//! the right shape for something that must never keep the app from booting. This is not a pattern
//! for service replies: [`caps`] uses `serde_json` and strict types because uncertainty there is a
//! playback-safety decision. The capability query deliberately does not fill a missing webOS
//! version; no public version key or anonymous permission for one is evidenced.
use crate::tv::device::{Hardware, Info, NyxFile};

pub mod jail_repair;
pub mod caps;
pub mod toast;

const OS_INFO: &str = "/var/run/nyx/os_info.json";

/// Pull `"key": "value"` out of a flat JSON object. Returns None rather than erroring: nothing
/// here is worth failing a boot over.
fn field<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let at = s.find(&format!("\"{key}\""))?;
    let rest = &s[at + key.len() + 2..];
    let colon = rest.find(':')?;
    let open = rest[colon..].find('"')? + colon + 1;
    let close = rest[open..].find('"')? + open;
    Some(&rest[open..close])
}

/// Everything [`probe`] extracts, as a pure function of the file's text — so the parse is testable
/// without a filesystem, and an unreadable file and an unparseable one land on the same value.
fn parse(s: &str) -> Info {
    let get = |k: &str| field(s, k).unwrap_or_default().to_string();
    let release = get("webos_release");
    let major = release
        .split('.')
        .next()
        .and_then(|m| m.parse::<u32>().ok())
        .unwrap_or(0);
    Info {
        release,
        codename: get("webos_release_codename"),
        api: get("webos_api_version"),
        name: get("webos_name"),
        major,
    }
}

/// Read it once and log it. Called at boot; safe to call when the file does not exist.
pub fn probe() {
    let text = match std::fs::read_to_string(OS_INFO) {
        Ok(s) => Some(s),
        Err(e) => {
            plx_base::eventlog::log(&format!(
                "webos: {OS_INFO} unreadable ({e}) — version unknown"
            ));
            None
        }
    };
    let (info, os_info) = os_info_from(text.as_deref());
    if info.major > 0 {
        plx_base::eventlog::log(&format!(
            "webos: {} release={} codename={} api={} major={}",
            info.name, info.release, info.codename, info.api, info.major
        ));
    }
    crate::tv::device::publish_info(info);
    let device_info = probe_hw();
    crate::tv::device::publish_probe(crate::tv::device::Probe { os_info, device_info });
    probe_jail();
}

// ---- which SET this is, as opposed to which webOS ---------------------------------------------

/// nyx's other file. Same directory, same flat shape, written by the same platform component.
const DEVICE_INFO: &str = "/var/run/nyx/device_info.json";

/// Pure, and tolerant of which spelling a firmware uses: nyx has carried both snake_case and
/// camelCase for these keys across releases, and we have exactly one device to check against — so
/// each field takes the first spelling that answers rather than betting on one.
fn parse_hw(s: &str) -> Hardware {
    let first = |keys: &[&str]| -> String {
        keys.iter()
            .find_map(|k| field(s, k))
            .unwrap_or_default()
            .to_string()
    };
    Hardware {
        model: first(&["modelName", "model_name", "device_name"]),
        board: first(&["boardType", "board_type", "platform_code", "chip_name"]),
        hw_revision: first(&["hardware_revision", "hardwareRevision", "hardware_version"]),
    }
}

/// PURE: [`parse`] plus what the read said. `None` is a file that could not be read.
fn os_info_from(text: Option<&str>) -> (Info, NyxFile) {
    let Some(text) = text else {
        return (Info::default(), NyxFile::Unreadable);
    };
    let info = parse(text);
    let answered = !info.release.is_empty()
        || !info.codename.is_empty()
        || !info.api.is_empty()
        || !info.name.is_empty();
    (info, if answered { NyxFile::Ok } else { NyxFile::NoKeys })
}

/// PURE: the SoC class from `/etc/prefs/properties/machineName`, for a set whose
/// `device_info.json` names none. The file is a flat text value whose leading run of lowercase
/// letters and digits is the board family (`k5lp`, `k3lp`, `o22`…), the same prefix
/// [`is_realtek_k5lp_family`] matches; anything after it (a variant suffix, a separator, a build
/// tag) is not reported. Bounded to eight characters and ASCII alphanumerics so the result can
/// only ever be a short class name, never free text; empty when the name does not start like one.
fn soc_from_machine_name(machine_name: &str) -> String {
    let class: String = machine_name
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .take(8)
        .collect();
    if class.starts_with(|c: char| c.is_ascii_lowercase()) {
        class
    } else {
        String::new()
    }
}

/// PURE: [`parse_hw`] plus what the read said, with the SoC taken from the machine name when
/// `device_info.json` gave none. The status describes the FILE, so a set whose soc was recovered
/// from the machine name still reads `no_keys`.
fn hardware_from(text: Option<&str>, machine_name: &str) -> (Hardware, NyxFile) {
    let Some(text) = text else {
        let hw = Hardware { board: soc_from_machine_name(machine_name), ..Hardware::default() };
        return (hw, NyxFile::Unreadable);
    };
    let mut hw = parse_hw(text);
    let answered = !hw.model.is_empty() || !hw.board.is_empty() || !hw.hw_revision.is_empty();
    if hw.board.is_empty() {
        hw.board = soc_from_machine_name(machine_name);
    }
    (hw, if answered { NyxFile::Ok } else { NyxFile::NoKeys })
}

/// Read it once and log it. Called from [`probe`], so it shares that call's one boot slot. Returns
/// what the read found, for [`probe`] to publish beside the other file's.
fn probe_hw() -> NyxFile {
    let text = match std::fs::read_to_string(DEVICE_INFO) {
        Ok(s) => Some(s),
        Err(e) => {
            plx_base::eventlog::log(&format!(
                "webos: {DEVICE_INFO} unreadable ({e}) — model/board unknown"
            ));
            None
        }
    };
    let (hw, status) = hardware_from(text.as_deref(), &read_machine_name());
    if !hw.model.is_empty() || !hw.board.is_empty() {
        plx_base::eventlog::log(&format!(
            "webos: model={} board={} hw={}",
            hw.model, hw.board, hw.hw_revision
        ));
    }
    crate::tv::device::publish_hardware(hw);
    status
}

// ---- k5lp/k3lp jail pre-flight: does this jail have /dev/rtkmem? ------------------------------
//
// **Community-tier evidence, not an LG or webosbrew formal spec.** On 2019-model Realtek-SoC
// webOS 4.x sets whose board name starts `k5lp` or `k3lp`, the DEFAULT jailer profile
// (`native_devmode` — exactly how this app is installed) omits `/dev/rtkmem`, a device node the
// video-output init path needs. Sources: webosbrew/webos-homebrew-channel PR #202 ("For 2019
// models with k5lp SoC, default jailer config (native_devmode) misses /dev/rtkmem causing A/V
// related apps to crash (e.g. Kodi, Moonlight)"), and ss4s's own detection
// (`modules/webos/utils/jail_check.c`), which reads exactly the file below then probes exactly
// the path below. A GitHub PR plus another open-source project's own probe is real, but it is
// still one project's finding rather than a vendor statement — every log line and read-out
// sentence built on this states what was FOUND, never asserts a diagnosis as certain fact.

/// A flat text value (unlike the two JSON files above) — one line naming the board/SoC family.
const MACHINE_NAME: &str = "/etc/prefs/properties/machineName";

/// The device node whose absence is the reported root cause. See the section doc above.
const RTKMEM_DEVICE: &str = "/dev/rtkmem";

/// The trimmed machine name, or empty when the file cannot be read. Shared by the jail pre-flight
/// and the hardware probe's SoC fallback.
fn read_machine_name() -> String {
    std::fs::read_to_string(MACHINE_NAME)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// PURE: does this machine name belong to the affected 2019 Realtek family? Exact PREFIX match —
/// the two SoC name spellings webosbrew's PR names, and nothing broader: a name that merely
/// *contains* one of them without starting with it does not match, and neither does a case
/// variant this platform has never been observed to write.
pub fn is_realtek_k5lp_family(machine_name: &str) -> bool {
    machine_name.starts_with("k5lp") || machine_name.starts_with("k3lp")
}

/// Read the machine name and, only on a matching SoC, probe `/dev/rtkmem` with `access(2)` —
/// once per boot, from [`probe`]'s own boot slot, so it shares that call's ordering guarantee
/// (before the first Load can be attempted). Logs UNCONDITIONALLY, beside the `webos:` line:
/// `devjail: soc=<name> rtkmem=ok|missing|n/a`. `<name>` is the raw, trimmed file content, or
/// empty when the file could not be read — logged honestly rather than invented, the same rule
/// [`probe`] and [`probe_hw`] already follow for their own unreadable files.
fn probe_jail() {
    let name = read_machine_name();
    let result = if is_realtek_k5lp_family(&name) {
        let readable = std::ffi::CString::new(RTKMEM_DEVICE)
            .map(|path| unsafe { libc::access(path.as_ptr(), libc::R_OK) } == 0)
            .unwrap_or(false);
        if readable {
            crate::tv::sandbox::Verdict::Ok
        } else {
            crate::tv::sandbox::Verdict::Missing
        }
    } else {
        crate::tv::sandbox::Verdict::NotApplicable
    };
    crate::tv::sandbox::publish(result);
    plx_base::eventlog::log(&format!("devjail: soc={name} rtkmem={}", crate::tv::sandbox::context()));
}

// ---- the ROOT press: give the screen back, without ending the process -------------------------

/// **Show the television's own Home, and keep running.**
///
/// This is what BACK does when the app's navigation has nowhere left to go — see `app.rs`'s
/// `back_at_root`. It is NOT a quit: the process stays alive, webOS backgrounds it (`0x103`/`0x104`
/// reach the SDL loop, and `0x105`/`0x106` when the launcher tile brings it back), and the app
/// comes back as the SAME LIVE PROCESS. Not necessarily to the same picture, and not necessarily to
/// the same route: workers keep running while backgrounded, so a sign-in that was mid-flow may have
/// completed and routed on, and one this press had to restart (`app.rs::after_cancel`) shows a
/// fresh QR by design. The contract is the process, and everything else is whatever the app would
/// have been doing had nobody pressed anything.
///
/// # Why this is the platform's answer and not ours
///
/// LG's own back-button guide states the behaviour the platform performs when BACK is pressed at
/// an app's entry page: *"a popup asking whether to exit the app is displayed on webOS TV 6.0 or
/// higher, or the **Home launcher is launched** on webOS TV 5.0 or lower"*
/// (<https://webostv.developer.lge.com/develop/guides/back-button>, LG vendor documentation — it
/// describes the WEB runtime's `webOS.platformBack()`, which a native app cannot call, but it is
/// LG stating what the ROOT press means on this platform). The dev set is 4.10.2, so "launch the
/// Home launcher" is the behaviour to reproduce, and the app's previous "Exit PlxNative?" question
/// was the webOS 6+ shape applied to a webOS 4 television — and it quit, which even there is only
/// one of the two answers.
///
/// # The two mechanisms, in the order they are tried
///
/// 1. **SAM.** `luna://com.webos.applicationManager/launch` with the launcher's id. Same service
///    and method this repo already drives against this television from the Makefile and
///    `tools/tv-session.sh` (`launch`, `closeByAppId`), so the bus, the method and the payload
///    shape are device-proven; only the id is new. **Device-verified 2026-09-04 (webOS 4.10.2):**
///    `SAM accepted in 16ms`, and the launcher comes up as the RIBBON over the app — the app stays
///    on screen behind it, keeps drawing, keeps its pid, receives NO `LIFECYCLE: background`, and a
///    SAM `launch` of the app's own id brings it back with no ribbon. That is what the HOME key
///    does on this firmware, so it is the behaviour; this doc used to say launching Home
///    "backgrounds us", which is the webOS 5+ full-screen launcher's shape and not this set's. It
///    took a plain anonymous `LSRegister` to get there — [`ls2`]'s module doc has the table of
///    what the hub answers to each registration shape.
/// 2. **Minimize the surface.** `SDL_MinimizeWindow`, which on LG's SDL fork reaches the webOS
///    shell. The fork does bind that protocol: the harvested `libSDL2-2.0.so.0.4.1` off this set
///    carries `wl_webos_shell`, `wl_webos_shell_interface` and `wl_webos_shell_surface_interface`
///    in `.rodata`, and it is how `SDL_WEBOS_ACCESS_POLICY_KEYS_BACK` (set by `src/main.c`) becomes
///    the surface property that lets BACK reach this app at all. `wl_webos_shell_surface.set_state`
///    with `WL_WEBOS_SHELL_SURFACE_STATE_MINIMIZED = 1` is LG's own protocol
///    (`wayland-webos-shell-client-protocol.h` / `webos-shell.xml` in the NDK sysroot), and it is
///    what Kodi's webOS port drives from its `SetMinimized`. What is NOT proven from the binary is
///    that LG's fork wires `SDL_MinimizeWindow` to it — the library is stripped and the driver's
///    hook is an internal function pointer — so this is the FALLBACK and not the mechanism: SDL's
///    own contract makes an unimplemented hook a silent no-op, which is the correct failure here.
///
/// If BOTH legs fail the user stays exactly where they were, which is the right last resort: the
/// alternative is ending the process, and "must not terminate the application" is the whole of what
/// was asked for. `SDL_MinimizeWindow` returns void, so the app cannot even know the second leg
/// failed — the log line says what was ASKED, never that it worked, and the screenshot is what
/// says whether it did.
///
/// `/tmp/plxnative-gohome=<sam|minimize>` forces one leg, so both can be exercised in one device
/// session instead of only whichever happens to answer first. Compiled out under `RELEASE=1`.
///
/// **Not rate-limited here** — [`crate::tv::home::take_root_press`] is, and `app.rs` claims the
/// press through it before it does anything else, `auth::cancel` included. Call this only having
/// claimed one.
pub fn go_home() {
    let forced = plx_base::devtrig::read("gohome");
    let forced = forced.as_deref().map(str::trim).unwrap_or("");
    let mode = match forced {
        "sam" | "minimize" | "probe" => forced,
        _ => "auto",
    };
    // The FIRST line of every root press, and the one that makes the rest of them readable: which
    // legs are even eligible. Without it a reader cannot tell a forced run from an ordinary one,
    // and the device evidence for this change is read by somebody who did not write it.
    plx_base::eventlog::log(&format!("gohome: request mode={mode}"));
    if mode == "minimize" { minimize(); return; }
    if HOME_PENDING.swap(true, std::sync::atomic::Ordering::AcqRel) { return; }
    let probe = mode == "probe";
    let sam_only = mode == "sam";
    if !plx_base::task::spawn_small("platform home", move || {
        if probe { ls2_probe(); }
        else if !launch_home() {
            if sam_only { plx_base::eventlog::log("gohome: no fallback — the trigger forced SAM only"); }
            else { HOME_MINIMIZE.store(true, std::sync::atomic::Ordering::Release); }
        }
        HOME_PENDING.store(false, std::sync::atomic::Ordering::Release);
    }) {
        HOME_PENDING.store(false, std::sync::atomic::Ordering::Release);
        if !probe && !sam_only { minimize(); }
    }
}

static HOME_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static HOME_MINIMIZE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// LS2 waits on its own context off-thread; only the SDL fallback returns to the frame thread.
pub fn poll_home() {
    if HOME_MINIMIZE.swap(false, std::sync::atomic::Ordering::AcqRel) { minimize(); }
}

/// The Home launcher's app id.
///
/// Community tier (webosbrew's commands cheatsheet and the widely copied
/// `luna-send … /launch '{"id":"com.webos.app.home"}'` recipe), which is why the reply is graded
/// rather than assumed: a wrong id comes back `returnValue: false` and falls through to the
/// minimize leg instead of leaving the user on a screen whose BACK did nothing.
const HOME_APP_ID: &str = "com.webos.app.home";

fn launch_home() -> bool {
    plx_base::eventlog::log(&format!(
        "gohome: no LS2 bus off-device — the root press would launch {HOME_APP_ID} on a television"
    ));
    true
}

fn minimize() {}

fn ls2_probe() {
    plx_base::eventlog::log("gohome: no LS2 bus off-device — nothing to probe");
}


/// Grade only the allowlisted fields in the LS2 wake reply. The raw platform JSON never enters a
/// report or the local snapshot.
#[cfg(test)]
fn storage_activation_reply(reply: &str) -> crate::storage::wire::failure::Detail {
    use crate::storage::wire::failure::{Detail, Stage};
    if reply.len() > 4096 { return Detail::new(Stage::ActivationInvalidReply, None); }
    let value = serde_json::from_str::<serde_json::Value>(reply).ok();
    let accepted = value.as_ref().and_then(|value| value.get("returnValue"))
        .and_then(serde_json::Value::as_bool);
    let code = value.as_ref().and_then(|value| value.get("errorCode"))
        .and_then(serde_json::Value::as_i64)
        .and_then(|value| i32::try_from(value).ok());
    match accepted {
        Some(true) => Detail::new(Stage::ActivationAccepted, code),
        Some(false) => Detail::new(Stage::ActivationRejected, code),
        None => Detail::new(Stage::ActivationInvalidReply, code),
    }
}


/// Hosts and the simulator have no storage helper to wake: the same answer an absent port gives.
pub fn activate_storage_helper(_service: &str) -> crate::storage::wire::failure::Detail {
    use crate::storage::wire::failure::{Detail, Stage};
    Detail::new(Stage::Unsupported, None)
}


pub fn system_locale() -> crate::tv::LocaleReply {
    crate::tv::LocaleReply::NoPlatform
}






pub fn bind_window(_win: *mut std::os::raw::c_void) {}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_activation_reply_keeps_only_status_and_numeric_error_code() {
        use crate::storage::wire::failure::{Detail, Stage};
        assert_eq!(storage_activation_reply(r#"{"returnValue":true}"#),
            Detail::new(Stage::ActivationAccepted, None));
        assert_eq!(storage_activation_reply(
            r#"{"returnValue":false,"errorCode":-1,"errorText":"private refusal"}"#),
            Detail::new(Stage::ActivationRejected, Some(-1)));
        assert_eq!(storage_activation_reply("not json"),
            Detail::new(Stage::ActivationInvalidReply, None));
        assert_eq!(storage_activation_reply(&"x".repeat(4097)),
            Detail::new(Stage::ActivationInvalidReply, None));
    }

    /// The real file off the dev set, verbatim. The parser has to survive the platform's
    /// formatting, not a tidied version of it.
    const REAL: &str = r#"{
    "core_os_kernel_version": "4.4.84-169.gld4tv.5",
    "core_os_name": "Rockhopper",
    "core_os_release": "4.10.2-31",
    "core_os_release_codename": "goldilocks2-grampians",
    "encryption_key_type": "prodkey",
    "webos_api_version": "4.1.0",
    "webos_build_datetime": "20250827000043",
    "webos_name": "webOS TV",
    "webos_prerelease": "",
    "webos_release": "4.10.2",
    "webos_release_codename": "goldilocks2-grampians"
}"#;

    #[test]
    fn reads_the_dev_sets_real_os_info() {
        assert_eq!(field(REAL, "webos_release"), Some("4.10.2"));
        assert_eq!(
            field(REAL, "webos_release_codename"),
            Some("goldilocks2-grampians")
        );
        assert_eq!(field(REAL, "webos_api_version"), Some("4.1.0"));
        assert_eq!(field(REAL, "webos_name"), Some("webOS TV"));
    }

    /// `webos_release` must not be satisfied by `core_os_release`, which appears FIRST in the file
    /// and whose value ("4.10.2-31") differs. A substring search that ignored the quotes would
    /// return the wrong field on every real device.
    #[test]
    fn does_not_match_a_longer_key_that_contains_it() {
        assert_ne!(field(REAL, "webos_release"), field(REAL, "core_os_release"));
        assert_eq!(field(REAL, "core_os_release"), Some("4.10.2-31"));
    }

    /// An empty value is a value, not a miss — `webos_prerelease` is empty on a shipping set.
    #[test]
    fn an_empty_string_value_parses_as_empty() {
        assert_eq!(field(REAL, "webos_prerelease"), Some(""));
    }

    /// Garbage in must not panic: this runs during boot, before anything is on screen.
    #[test]
    fn malformed_input_is_none_not_a_panic() {
        for bad in [
            "",
            "{",
            "{\"webos_release\"",
            "{\"webos_release\":",
            "not json at all",
        ] {
            assert_eq!(field(bad, "webos_release"), None, "input {bad:?}");
        }
    }

    /// Both spellings of the hardware keys resolve, because we have one television to check
    /// against and nyx has carried both across releases.
    #[test]
    fn the_hardware_record_takes_whichever_spelling_the_firmware_uses() {
        let snake =
            r#"{"model_name":"49SM9000PLA","board_type":"HE_DTV_W19H","hardware_revision":"1.0"}"#;
        let camel = r#"{"modelName":"OLED55C1","boardType":"k8hp","hardwareRevision":"2.0"}"#;
        assert_eq!(parse_hw(snake).model, "49SM9000PLA");
        assert_eq!(parse_hw(snake).board, "HE_DTV_W19H");
        assert_eq!(parse_hw(camel).model, "OLED55C1");
        assert_eq!(parse_hw(camel).board, "k8hp");
        assert_eq!(parse_hw(camel).hw_revision, "2.0");
    }

    /// Unknown is EMPTY, never a guess — and never a panic, since this runs during boot.
    #[test]
    fn an_unreadable_device_info_yields_an_empty_record() {
        let hw = parse_hw("not json at all");
        assert!(hw.model.is_empty() && hw.board.is_empty() && hw.hw_revision.is_empty());
    }

    /// PLX-NATIVE-15's set: both nyx files yielded nothing, so `model`, `soc` and the webOS release
    /// were empty strings, indistinguishable from a probe that never ran. The SoC is still
    /// recoverable from the machine name the jail pre-flight already reads.
    #[test]
    fn a_device_info_without_keys_takes_its_soc_from_the_machine_name() {
        let (hw, status) = hardware_from(Some("{}"), "k5lp");
        assert_eq!(hw.board, "k5lp");
        assert!(hw.model.is_empty() && hw.hw_revision.is_empty(), "nothing else is invented");
        assert_eq!(status, NyxFile::NoKeys, "the status describes the file, not the fallback");
        // A longer real-looking name reports its family prefix only.
        assert_eq!(hardware_from(Some("{}"), "k5lp.grampians").0.board, "k5lp");
        assert_eq!(hardware_from(Some("{}"), "k3lp-0042 extra").0.board, "k3lp");
        // Never overrides what the file said.
        let named = r#"{"modelName":"OLED55C1","boardType":"k8hp"}"#;
        let (hw, status) = hardware_from(Some(named), "k5lp");
        assert_eq!((hw.board.as_str(), hw.model.as_str()), ("k8hp", "OLED55C1"));
        assert_eq!(status, NyxFile::Ok);
    }

    #[test]
    fn the_soc_class_is_a_short_ascii_token_or_nothing() {
        assert_eq!(soc_from_machine_name(""), "");
        assert_eq!(soc_from_machine_name("  o22n3 \n"), "o22n3");
        assert_eq!(soc_from_machine_name("123abc"), "", "a name must start like a board class");
        assert_eq!(soc_from_machine_name("K5LP"), "");
        assert_eq!(soc_from_machine_name("abcdefghijklmnop"), "abcdefgh", "bounded to eight");
        assert_eq!(soc_from_machine_name("k5lp\u{e9}"), "k5lp");
    }

    #[test]
    fn an_unreadable_device_info_reports_unreadable_and_still_recovers_the_soc() {
        let (hw, status) = hardware_from(None, "k3lp");
        assert_eq!(status, NyxFile::Unreadable);
        assert_eq!(hw.board, "k3lp");
        let (hw, status) = hardware_from(None, "");
        assert_eq!(status, NyxFile::Unreadable);
        assert!(hw.board.is_empty() && hw.model.is_empty());
    }

    #[test]
    fn os_info_reports_ok_no_keys_or_unreadable() {
        assert_eq!(os_info_from(Some(REAL)).1, NyxFile::Ok);
        assert_eq!(os_info_from(Some("{}")).1, NyxFile::NoKeys);
        assert_eq!(os_info_from(Some("not json at all")).1, NyxFile::NoKeys);
        let (info, status) = os_info_from(None);
        assert_eq!(status, NyxFile::Unreadable);
        assert_eq!(info.major, 0);
        assert!(info.release.is_empty());
    }

    /// Absent key, present file.
    #[test]
    fn a_missing_key_is_none() {
        assert_eq!(field(REAL, "no_such_key"), None);
    }

    #[test]
    fn parses_the_whole_record() {
        let i = parse(REAL);
        assert_eq!((i.release.as_str(), i.major), ("4.10.2", 4));
        assert_eq!(i.codename, "goldilocks2-grampians");
        assert_eq!(i.name, "webOS TV");
    }

    /// The unknown case must be EMPTY with major 0, never a plausible-looking default. A panel that
    /// invents "4.0" for a set it could not read is worse than one that admits it does not know.
    #[test]
    fn an_unreadable_file_yields_no_version_rather_than_a_guess() {
        let i = parse("not json at all");
        assert_eq!(i.major, 0);
        assert!(i.release.is_empty() && i.codename.is_empty());
    }

    /// A two-component release still yields its major — LG has shipped "6.0" as well as "4.10.2".
    #[test]
    fn a_short_release_still_gives_a_major() {
        assert_eq!(parse(r#"{"webos_release": "6.0"}"#).major, 6);
        assert_eq!(parse(r#"{"webos_release": "10.0.1"}"#).major, 10);
    }

    /// The predicate the jail pre-flight gates on. Exact-prefix, and nothing broader — see the
    /// function's own doc for why "contains" is deliberately excluded.
    #[test]
    fn is_realtek_k5lp_family_matches_only_the_named_prefixes() {
        assert!(is_realtek_k5lp_family("k5lp"));
        assert!(is_realtek_k5lp_family("k3lp"));
        // A longer, real-looking board name still matches on the prefix alone.
        assert!(is_realtek_k5lp_family("k5lp.grampians"));
        assert!(is_realtek_k5lp_family("k3lp.something"));
        // Not a match: a different board entirely, empty, or the family name present but not
        // leading (which is not the shape any real machineName has taken).
        assert!(!is_realtek_k5lp_family("o22"));
        assert!(!is_realtek_k5lp_family(""));
        assert!(!is_realtek_k5lp_family("prefix-k5lp"));
        assert!(!is_realtek_k5lp_family("K5LP"));
    }
}
