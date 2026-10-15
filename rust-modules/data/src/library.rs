//! **Where the library reads from: its folders and SMB shares, and whether each is there now.**
//!
//! The library's top level ([`roots`]) is the list of [`Source`]s that are available, in the
//! order they are written. Where the list comes from, first match wins ([`Origin`]):
//!
//! 1. `PITV_MEDIA_DIRS` (`:`-separated absolute folders, like `PATH`) — a run that sets it
//!    (the headless harness, a one-off launch) reads nothing else;
//! 2. the config file ([`config_path`]): `$PITV_CONFIG`, else `$XDG_CONFIG_HOME/pitv/library.conf`,
//!    else `~/.config/pitv/library.conf`;
//! 3. `~/Videos`.
//!
//! # The config file
//!
//! One section per source; its name is the source's tab label. There is no on-screen editor: the
//! file is edited by hand and Settings › Library › Rescan reads it again.
//!
//! ```ini
//! # a folder on this machine (or a mount fstab already made)
//! [Films]
//! path = /srv/media/Films
//!
//! # an SMB share, mounted through gio (gvfs) when the app starts
//! [TV Shows]
//! share = smb://nas/media/TV Shows
//! user = gavin
//! domain = WORKGROUP                       # optional
//! password-file = ~/.config/pitv/nas.pass  # or: password = ...
//! ```
//!
//! A share without `user` or a password is mounted anonymously. Lines the parser cannot use are
//! reported ([`problems`]: Settings names the first, the event log every one), never fatal: the
//! sections that read cleanly still load.
//!
//! # Checked off the frame
//!
//! Every source is checked on a worker — a folder by `stat` (it may itself be a network mount that
//! hangs), a share by mounting it ([`gio`]) — and lands as a [`Status`]. Only a [`Status::Ready`]
//! source is a root, so a share's tab appears once it is connected. Each landing bumps
//! [`revision`] and wakes the frame loop; the tab bar reads [`roots`] every frame.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

mod gio;

/// The config file's name inside `pitv/` under the config home.
pub const CONFIG_FILE: &str = "library.conf";

/// **One place the library reads from**: its label and where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub name: String,
    pub place: Place,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// A folder on this machine's filesystem.
    Folder(PathBuf),
    Share(Share),
}

/// An SMB share as the config names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    /// `smb://server/share[/folder…]`, as written.
    pub uri: String,
    pub user: Option<String>,
    pub domain: Option<String>,
    pub password: Option<Password>,
}

/// A share's password: written in the config, or the first line of a file beside it.
#[derive(Clone, PartialEq, Eq)]
pub enum Password {
    Inline(String),
    File(PathBuf),
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Password::Inline(_) => f.write_str("Inline(..)"),
            Password::File(p) => write!(f, "File({})", p.display()),
        }
    }
}

/// Whether a source can be listed now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// The worker has not answered yet (a share: it is being mounted).
    Checking,
    /// Listable at this local path: the folder itself, or where gvfs mounted the share.
    Ready(PathBuf),
    /// A folder that is not there (or is not a folder).
    Missing,
    /// A share that could not be mounted, with gio's reason.
    Failed(String),
}

/// Where the source list came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// `PITV_MEDIA_DIRS`; the config file at the path is not read.
    Env(PathBuf),
    /// The config file at this path.
    File(PathBuf),
    /// Neither: `~/Videos`. The path is where a config file would be read from.
    Default(PathBuf),
}

impl Origin {
    /// The config file this origin reads or would read.
    pub fn config_path(&self) -> &Path {
        match self {
            Origin::Env(p) | Origin::File(p) | Origin::Default(p) => p,
        }
    }
}

/// A source and what is known of it now — Settings' row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceView {
    pub source: Source,
    pub status: Status,
}

/// What [`parse`] read: the sources it could use, and a line-numbered note per line it could not.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub sources: Vec<Source>,
    pub problems: Vec<String>,
}

/// **Read a config file's text.** `home` expands a leading `~/` in `path` and `password-file`.
pub fn parse(text: &str, home: Option<&Path>) -> Parsed {
    #[derive(Default)]
    struct Section {
        name: String,
        line: usize,
        path: Option<String>,
        share: Option<String>,
        user: Option<String>,
        domain: Option<String>,
        password: Option<Password>,
    }
    let expand = |v: &str| match (v.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(v),
    };
    let mut out = Parsed::default();
    let finish = |s: Section, out: &mut Parsed| {
        let what = format!("line {}: [{}]", s.line, s.name);
        let place = match (s.path, s.share) {
            (Some(path), None) => {
                let path = expand(&path);
                if !path.is_absolute() {
                    out.problems.push(format!("{what}: `path` must be an absolute folder"));
                    return;
                }
                if s.user.is_some() || s.domain.is_some() || s.password.is_some() {
                    out.problems.push(format!("{what}: `user`, `domain` and passwords belong to a `share`"));
                }
                Place::Folder(path)
            }
            (None, Some(uri)) => {
                if !share_uri_ok(&uri) {
                    out.problems.push(format!("{what}: `share` must read smb://server/share"));
                    return;
                }
                Place::Share(Share { uri, user: s.user, domain: s.domain, password: s.password })
            }
            (Some(_), Some(_)) => {
                out.problems.push(format!("{what}: has both `path` and `share`; give one"));
                return;
            }
            (None, None) => {
                out.problems.push(format!("{what}: needs `path = /folder` or `share = smb://server/share`"));
                return;
            }
        };
        out.sources.push(Source { name: s.name, place });
    };
    let mut current: Option<Section> = None;
    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            if let Some(done) = current.take() {
                finish(done, &mut out);
            }
            let name = name.trim();
            if name.is_empty() {
                out.problems.push(format!("line {n}: a section needs a name, like [Films]"));
                continue;
            }
            current = Some(Section { name: name.to_owned(), line: n, ..Default::default() });
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            out.problems.push(format!("line {n}: expected `key = value` or [Name]"));
            continue;
        };
        let (key, value) = (key.trim(), unquote(value.trim()));
        let Some(s) = current.as_mut() else {
            out.problems.push(format!("line {n}: `{key}` comes before any [Name] section"));
            continue;
        };
        let slot = match key {
            "path" => &mut s.path,
            "share" => &mut s.share,
            "user" => &mut s.user,
            "domain" => &mut s.domain,
            "password" => {
                s.password = Some(Password::Inline(value.to_owned()));
                continue;
            }
            "password-file" => {
                s.password = Some(Password::File(expand(value)));
                continue;
            }
            other => {
                out.problems.push(format!("line {n}: unknown key `{other}`"));
                continue;
            }
        };
        *slot = (!value.is_empty()).then(|| value.to_owned());
    }
    if let Some(done) = current.take() {
        finish(done, &mut out);
    }
    out
}

/// `"value"` → `value`; anything else as it is.
fn unquote(v: &str) -> &str {
    v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v)
}

/// `smb://server/share…`: a server and a share name, at least.
fn share_uri_ok(uri: &str) -> bool {
    let Some(rest) = uri.strip_prefix("smb://") else { return false };
    let mut parts = rest.split('/');
    matches!((parts.next(), parts.next()), (Some(server), Some(share)) if !server.is_empty() && !share.is_empty())
}

/// The sources `PITV_MEDIA_DIRS` names: each absolute folder, labelled by its own name. Empty
/// when it is unset or names none.
fn from_env(env: Option<&std::ffi::OsStr>) -> Vec<Source> {
    let Some(list) = env else { return Vec::new() };
    std::env::split_paths(list).filter(|p| p.is_absolute()).map(folder_source).collect()
}

fn folder_source(path: PathBuf) -> Source {
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    Source { name, place: Place::Folder(path) }
}

/// The config file's path: `$PITV_CONFIG`, else under `$XDG_CONFIG_HOME`, else `~/.config`.
pub fn config_path() -> PathBuf {
    config_path_from(
        std::env::var_os("PITV_CONFIG").as_deref(),
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn config_path_from(explicit: Option<&std::ffi::OsStr>, xdg: Option<&std::ffi::OsStr>, home: Option<&std::ffi::OsStr>) -> PathBuf {
    if let Some(p) = explicit.filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let base = match xdg.filter(|x| !x.is_empty()).map(PathBuf::from).filter(|x| x.is_absolute()) {
        Some(x) => x,
        None => home.map_or_else(|| PathBuf::from("/"), PathBuf::from).join(".config"),
    };
    base.join("pitv").join(CONFIG_FILE)
}

/// **Decide the source list**: the environment's, else the file's, else `~/Videos`.
/// `read` reads the config file (`None`: there is none).
fn resolve(env: Option<&std::ffi::OsStr>, home: Option<&Path>, config: PathBuf, read: impl FnOnce(&Path) -> Option<String>) -> (Origin, Parsed) {
    let env_sources = from_env(env);
    if !env_sources.is_empty() {
        return (Origin::Env(config), Parsed { sources: env_sources, problems: Vec::new() });
    }
    if let Some(text) = read(&config) {
        return (Origin::File(config), parse(&text, home));
    }
    let videos = home.map(|h| vec![folder_source(h.join("Videos"))]).unwrap_or_default();
    (Origin::Default(config), Parsed { sources: videos, problems: Vec::new() })
}

struct Entry {
    source: Source,
    status: Status,
    /// Names this entry's latest check; a worker's answer for an older one is dropped.
    check: u64,
}

struct State {
    loaded: bool,
    origin: Origin,
    entries: Vec<Entry>,
    problems: Vec<String>,
    roots: Arc<[PathBuf]>,
    revision: u64,
    next_check: u64,
}

fn state() -> MutexGuard<'static, State> {
    static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
        Mutex::new(State {
            loaded: false,
            origin: Origin::Default(PathBuf::new()),
            entries: Vec::new(),
            problems: Vec::new(),
            roots: Arc::new([]),
            revision: 0,
            next_check: 0,
        })
    });
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// **Read the source list and start checking every source.** At boot; [`rescan`] reads it again.
pub fn load() {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let (origin, parsed) = resolve(std::env::var_os("PITV_MEDIA_DIRS").as_deref(), home.as_deref(), config_path(), |p| {
        std::fs::read_to_string(p).ok()
    });
    let how = match &origin {
        Origin::Env(_) => "PITV_MEDIA_DIRS".to_owned(),
        Origin::File(p) => p.display().to_string(),
        Origin::Default(_) => "~/Videos (no config file)".to_owned(),
    };
    plx_base::eventlog::log(&format!("library: {} source(s) from {how}", parsed.sources.len()));
    for p in &parsed.problems {
        plx_base::eventlog::log(&format!("library: config {p}"));
    }
    install(origin, parsed, start);
}

/// Adopt a new source list. A share already connected under the same settings keeps its mount
/// (a rescan does not drop a playing film's share); everything else is checked afresh.
fn install(origin: Origin, parsed: Parsed, start: fn(u64, Source)) {
    let mut checks = Vec::new();
    {
        let mut st = state();
        let old = std::mem::take(&mut st.entries);
        for source in parsed.sources {
            let kept = old.iter().find(|e| {
                e.source == source && matches!(e.source.place, Place::Share(_)) && matches!(e.status, Status::Ready(_))
            });
            let entry = match kept {
                Some(e) => Entry { source, status: e.status.clone(), check: e.check },
                None => {
                    st.next_check += 1;
                    let check = st.next_check;
                    checks.push((check, source.clone()));
                    Entry { source, status: Status::Checking, check }
                }
            };
            st.entries.push(entry);
        }
        st.origin = origin;
        st.problems = parsed.problems;
        st.loaded = true;
        changed(&mut st);
    }
    for (check, source) in checks {
        start(check, source);
    }
}

/// Check one source on a worker and land the answer under `check`.
fn start(check: u64, source: Source) {
    let spawned = plx_base::task::spawn_small("library check", move || {
        let status = match &source.place {
            Place::Folder(path) => match std::fs::metadata(path) {
                Ok(m) if m.is_dir() => Status::Ready(path.clone()),
                _ => Status::Missing,
            },
            Place::Share(share) => match gio::mount(share, gio::PROGRAM) {
                Ok(local) => Status::Ready(local),
                Err(e) => Status::Failed(e),
            },
        };
        let line = match &status {
            Status::Ready(p) => format!("library: {} ready at {}", source.name, p.display()),
            Status::Missing => format!("library: {} not found", source.name),
            Status::Failed(e) => format!("library: {} not connected: {e}", source.name),
            Status::Checking => String::new(),
        };
        plx_base::eventlog::log(&line);
        land(check, status);
    });
    if !spawned {
        land(check, Status::Failed("could not start a worker".to_owned()));
    }
}

fn land(check: u64, status: Status) {
    let mut st = state();
    let Some(e) = st.entries.iter_mut().find(|e| e.check == check) else { return };
    e.status = status;
    changed(&mut st);
    drop(st);
    plx_machine::idle::invalidate();
}

/// Recompute the roots from the statuses, and move the revision.
fn changed(st: &mut State) {
    let mut roots: Vec<PathBuf> = Vec::new();
    for e in &st.entries {
        if let Status::Ready(p) = &e.status {
            if !roots.contains(p) {
                roots.push(p.clone());
            }
        }
    }
    st.roots = roots.into();
    st.revision += 1;
}

fn ensure_loaded() {
    if !state().loaded {
        load();
    }
}

/// **The library's top-level folders**: every source that is [`Status::Ready`], in config order.
pub fn roots() -> Arc<[PathBuf]> {
    ensure_loaded();
    state().roots.clone()
}

/// The label the config gives the root at `path` (its section name), when one does.
pub fn name_of(path: &Path) -> Option<String> {
    let st = state();
    st.entries.iter().find(|e| matches!(&e.status, Status::Ready(p) if p == path)).map(|e| e.source.name.clone())
}

/// Every source and its status, in config order.
pub fn sources() -> Vec<SourceView> {
    ensure_loaded();
    state().entries.iter().map(|e| SourceView { source: e.source.clone(), status: e.status.clone() }).collect()
}

/// Where the list came from.
pub fn origin() -> Origin {
    ensure_loaded();
    state().origin.clone()
}

/// The config lines that could not be used, each `line N: why`.
pub fn problems() -> Vec<String> {
    state().problems.clone()
}

/// Moves whenever a source, its status or the list changes.
pub fn revision() -> u64 {
    state().revision
}

/// **Check source `i` again**: mount a share that failed (or remount one that went away), look for
/// a folder that was missing.
pub fn reconnect(i: usize) {
    let (check, source) = {
        let mut st = state();
        st.next_check += 1;
        let check = st.next_check;
        let Some(e) = st.entries.get_mut(i) else { return };
        e.status = Status::Checking;
        e.check = check;
        let source = e.source.clone();
        changed(&mut st);
        (check, source)
    };
    plx_base::eventlog::log(&format!("library: reconnect {}", source.name));
    start(check, source);
}

/// **Read the config again and re-list the library**: new sections appear, removed ones go, and
/// every folder listing is read afresh the next time it shows.
pub fn rescan() {
    load();
    crate::local::forget_listings();
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# the library
[Films]
path = /srv/media/Films

[TV Shows]
share = smb://nas/media/TV Shows
user = gavin
domain = WORKGROUP
password-file = ~/.config/pitv/nas.pass

[Music videos]
share = "smb://nas/clips"
"#;

    #[test]
    fn a_config_names_folders_and_shares_in_order() {
        let parsed = parse(SAMPLE, Some(Path::new("/home/u")));
        assert_eq!(parsed.problems, Vec::<String>::new());
        assert_eq!(parsed.sources.len(), 3);
        assert_eq!(parsed.sources[0], Source { name: "Films".into(), place: Place::Folder("/srv/media/Films".into()) });
        assert_eq!(
            parsed.sources[1].place,
            Place::Share(Share {
                uri: "smb://nas/media/TV Shows".into(),
                user: Some("gavin".into()),
                domain: Some("WORKGROUP".into()),
                password: Some(Password::File("/home/u/.config/pitv/nas.pass".into())),
            })
        );
        assert_eq!(
            parsed.sources[2].place,
            Place::Share(Share { uri: "smb://nas/clips".into(), user: None, domain: None, password: None }),
            "quotes come off, and a share with no user is anonymous"
        );
    }

    #[test]
    fn lines_the_parser_cannot_use_are_named_and_the_rest_still_loads() {
        let text = "stray = 1\n[Good]\npath = /a\n[No place]\nuser = x\n[Relative]\npath = films\n[Both]\npath = /b\nshare = smb://s/x\n[Bad share]\nshare = //nas\n[Typo]\npaht = /c\n[]\njunk\n[Folder with user]\npath = /d\nuser = me\n";
        let parsed = parse(text, None);
        let names: Vec<&str> = parsed.sources.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Good", "Folder with user"]);
        let p = &parsed.problems;
        assert!(p[0].starts_with("line 1:") && p[0].contains("before any"), "{p:?}");
        assert!(p.iter().any(|l| l.contains("[No place]") && l.contains("needs")), "{p:?}");
        assert!(p.iter().any(|l| l.contains("[Relative]") && l.contains("absolute")), "{p:?}");
        assert!(p.iter().any(|l| l.contains("[Both]")), "{p:?}");
        assert!(p.iter().any(|l| l.contains("[Bad share]") && l.contains("smb://")), "{p:?}");
        assert!(p.iter().any(|l| l.contains("unknown key `paht`")), "{p:?}");
        assert!(p.iter().any(|l| l.contains("needs a name")), "{p:?}");
        assert!(p.iter().any(|l| l.contains("expected `key = value`")), "{p:?}");
        assert!(p.iter().any(|l| l.contains("[Folder with user]") && l.contains("belong to a `share`")), "{p:?}");
    }

    #[test]
    fn the_shipped_example_config_reads_cleanly() {
        let parsed = parse(include_str!("../../../tools/linux/library.conf.example"), Some(Path::new("/home/pi")));
        assert_eq!(parsed.problems, Vec::<String>::new());
        let names: Vec<&str> = parsed.sources.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Films", "TV Shows"]);
    }

    #[test]
    fn a_password_never_prints() {
        let p = Password::Inline("hunter2".into());
        assert!(!format!("{p:?}").contains("hunter2"));
    }

    #[test]
    fn the_environment_wins_then_the_file_then_videos() {
        let home = Path::new("/home/u");
        let cfg = PathBuf::from("/home/u/.config/pitv/library.conf");
        let file = |_: &Path| Some("[Films]\npath = /srv/f\n".to_owned());
        let none = |_: &Path| None;

        let (origin, parsed) = resolve(Some("/a:/b:relative".as_ref()), Some(home), cfg.clone(), file);
        assert_eq!(origin, Origin::Env(cfg.clone()));
        let names: Vec<_> = parsed.sources.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);

        let (origin, parsed) = resolve(Some("".as_ref()), Some(home), cfg.clone(), file);
        assert_eq!(origin, Origin::File(cfg.clone()), "an empty PITV_MEDIA_DIRS is no list");
        assert_eq!(parsed.sources[0].name, "Films");

        let (origin, parsed) = resolve(None, Some(home), cfg.clone(), none);
        assert_eq!(origin, Origin::Default(cfg));
        assert_eq!(parsed.sources, [Source { name: "Videos".into(), place: Place::Folder("/home/u/Videos".into()) }]);
    }

    #[test]
    fn the_config_file_is_found_by_pitv_config_then_xdg_then_home() {
        let os = |s: &'static str| Some(std::ffi::OsStr::new(s));
        assert_eq!(config_path_from(os("/etc/pitv.conf"), os("/x"), os("/home/u")), PathBuf::from("/etc/pitv.conf"));
        assert_eq!(config_path_from(None, os("/x"), os("/home/u")), PathBuf::from("/x/pitv/library.conf"));
        assert_eq!(config_path_from(os(""), os("relative"), os("/home/u")), PathBuf::from("/home/u/.config/pitv/library.conf"));
    }

    #[test]
    fn only_ready_sources_are_roots_and_a_rescan_keeps_a_connected_share() {
        let _g = plx_base::testlock::serial();
        let dir = plx_base::testscratch::process_dir("library-roots");
        let films = dir.join("Films");
        std::fs::create_dir_all(&films).unwrap();
        let share = Source {
            name: "NAS".into(),
            place: Place::Share(Share { uri: "smb://nas/x".into(), user: None, domain: None, password: None }),
        };
        let folder = Source { name: "Films".into(), place: Place::Folder(films.clone()) };
        let gone = Source { name: "Gone".into(), place: Place::Folder(dir.join("nope")) };
        // the share is never handed to gio here: only the folders are checked
        let folders_only: fn(u64, Source) = |check, source| {
            if matches!(source.place, Place::Folder(_)) {
                start(check, source);
            }
        };
        install(Origin::Default(dir.join("c")), Parsed { sources: vec![share.clone(), folder, gone], problems: vec![] }, folders_only);
        // pretend the share's mount landed, then wait for the two folder checks
        let share_check = state().entries[0].check;
        land(share_check, Status::Ready(dir.join("mnt")));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while sources().iter().any(|s| s.status == Status::Checking) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(sources()[2].status, Status::Missing);
        let pages_saw = crate::local::revision();
        reconnect(2);
        assert!(crate::local::revision() > pages_saw, "a page re-reads when a source changes");
        assert_eq!(&*roots(), [dir.join("mnt"), films.clone()]);
        assert_eq!(name_of(&films).as_deref(), Some("Films"));

        // the same share again keeps its mount; a removed folder leaves the roots
        install(Origin::Default(dir.join("c")), Parsed { sources: vec![share], problems: vec![] }, folders_only);
        assert_eq!(sources()[0].status, Status::Ready(dir.join("mnt")));
        assert_eq!(&*roots(), [dir.join("mnt")]);
        // a stale answer for a replaced check is dropped
        land(share_check + 100, Status::Missing);
        assert_eq!(sources()[0].status, Status::Ready(dir.join("mnt")));
        state().loaded = false;
    }
}
