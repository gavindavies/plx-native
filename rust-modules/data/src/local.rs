//! **The local library: folders of video files on this machine's own disk.**
//!
//! The Linux build's catalog, standing where a Plex server stands for the television: the folder
//! browser (`plx_screens::local`) reads directory listings from here, and the player plays the
//! files they name. There is no index and no scraper — a folder is listed when it is opened,
//! the way a file manager does it, so a file copied onto the disk is there the next time its
//! folder is shown.
//!
//! # Roots
//!
//! The library's top level is a list of folders ([`roots`]): `PITV_MEDIA_DIRS`
//! (`:`-separated, like `PATH`) when it is set, else `~/Videos`. A single root is shown as its own
//! contents; several are shown as one folder each.
//!
//! # Listings are scanned off the frame
//!
//! [`request`] starts a worker that reads one directory and every child folder's artwork, then
//! publishes the [`Listing`] and bumps [`revision`] (and wakes the frame loop), so a page asks on
//! mount and draws whatever [`listing`] holds — `Loading` until the scan lands. A listing older
//! than [`RESCAN_AFTER`] is scanned again when it is next requested; the old one keeps showing
//! until the new one lands, so a re-entered folder never blinks empty.
//!
//! # Artwork, the way media folders already carry it
//!
//! A folder's picture is `poster`/`folder`/`cover` (`.jpg`/`.jpeg`/`.png`, any case) inside it. A
//! video's is `<name>-poster.*` or `<name>.*` beside it, else its folder's own when it is the only
//! video there (the usual one-film-per-folder layout). The card asks for a picture by an opaque
//! token ([`Entry::art`]) that [`art_path`] turns back into the file — the poster pipeline keys
//! its slots by short strings, and a path can be any length.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The raw server id local artwork is requested under. No server is ever registered on the build
/// that has a local library, and the registry hands out ids from 0, so this one never names one.
pub const ART_SRV: u16 = u16::MAX - 1;

/// A listing this old is read again when its folder is next opened.
pub const RESCAN_AFTER: Duration = Duration::from_secs(10);

/// File extensions listed as videos (compared case-insensitively).
const VIDEO_EXTENSIONS: [&str; 9] = ["mkv", "mp4", "m4v", "mov", "ts", "m2ts", "avi", "webm", "mpg"];
const ART_EXTENSIONS: [&str; 3] = ["jpg", "jpeg", "png"];
const FOLDER_ART_NAMES: [&str; 3] = ["poster", "folder", "cover"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Folder,
    Video,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// What the card says: a folder's name, a video's name without its extension.
    pub title: String,
    pub path: PathBuf,
    pub kind: EntryKind,
    /// The artwork token for [`art_path`], if the entry has a picture.
    pub art: Option<String>,
    /// Bytes, for a video; 0 for a folder.
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Listing {
    /// Not scanned yet, or the first scan has not landed.
    Loading,
    Ready(Arc<Vec<Entry>>),
    /// The folder could not be read (gone, unmounted, no permission). Carries the OS's reason.
    Failed(String),
}

struct Scanned {
    listing: Listing,
    at: Instant,
    in_flight: bool,
}

#[derive(Default)]
struct State {
    dirs: HashMap<PathBuf, Scanned>,
    art_paths: Vec<PathBuf>,
    art_tokens: HashMap<PathBuf, usize>,
    revision: u64,
}

fn state() -> std::sync::MutexGuard<'static, State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default())).lock().unwrap_or_else(|e| e.into_inner())
}

/// The library's top-level folders. Read once: the environment does not change under a process.
pub fn roots() -> &'static [PathBuf] {
    static ROOTS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let roots = roots_from(std::env::var_os("PITV_MEDIA_DIRS"), std::env::var_os("HOME"));
        plx_base::eventlog::log(&format!("local: {} library folder(s)", roots.len()));
        roots
    })
}

fn roots_from(env: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> Vec<PathBuf> {
    if let Some(list) = env {
        let roots: Vec<PathBuf> = std::env::split_paths(&list).filter(|p| p.is_absolute()).collect();
        if !roots.is_empty() {
            return roots;
        }
    }
    home.map(|h| vec![PathBuf::from(h).join("Videos")]).unwrap_or_default()
}

/// The library's top level as a listing: the single root's own contents, or one folder per root.
/// `None` when it is one root (the caller lists that root instead).
pub fn top_level() -> Option<Arc<Vec<Entry>>> {
    let roots = roots();
    if roots.len() == 1 {
        return None;
    }
    let entries = roots
        .iter()
        .map(|root| Entry {
            title: root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned()),
            path: root.clone(),
            kind: EntryKind::Folder,
            art: folder_art(root).map(|p| art_token(&p)),
            size: 0,
        })
        .collect();
    Some(Arc::new(entries))
}

/// Moves whenever any listing lands; a page compares it to decide whether to re-read.
pub fn revision() -> u64 {
    state().revision
}

/// What is known of `dir` now. Does not start a scan — [`request`] does.
pub fn listing(dir: &Path) -> Listing {
    state().dirs.get(dir).map_or(Listing::Loading, |s| s.listing.clone())
}

/// Scan `dir` unless a scan is in flight or the last one is fresh.
pub fn request(dir: &Path) {
    {
        let mut st = state();
        let fresh = |s: &Scanned| s.in_flight || (s.at.elapsed() < RESCAN_AFTER && s.listing != Listing::Loading);
        match st.dirs.get_mut(dir) {
            Some(s) if fresh(s) => return,
            Some(s) => s.in_flight = true,
            None => {
                st.dirs.insert(dir.to_path_buf(), Scanned { listing: Listing::Loading, at: Instant::now(), in_flight: true });
            }
        }
    }
    let owned = dir.to_path_buf();
    let spawned = plx_base::task::spawn_small("local scan", move || {
        let listing = match scan(&owned) {
            Ok(entries) => Listing::Ready(Arc::new(entries)),
            Err(e) => {
                plx_base::eventlog::log(&format!("local: cannot list {}: {e}", owned.display()));
                Listing::Failed(e.to_string())
            }
        };
        let mut st = state();
        st.dirs.insert(owned, Scanned { listing, at: Instant::now(), in_flight: false });
        st.revision += 1;
        drop(st);
        plx_machine::idle::invalidate();
    });
    if !spawned {
        if let Some(s) = state().dirs.get_mut(dir) {
            s.in_flight = false;
        }
    }
}

/// The image file an artwork token names.
pub fn art_path(token: &str) -> Option<PathBuf> {
    let index: usize = token.strip_prefix('L')?.parse().ok()?;
    state().art_paths.get(index).cloned()
}

fn art_token(path: &Path) -> String {
    let mut st = state();
    if let Some(i) = st.art_tokens.get(path) {
        return format!("L{i}");
    }
    let i = st.art_paths.len();
    st.art_paths.push(path.to_path_buf());
    st.art_tokens.insert(path.to_path_buf(), i);
    format!("L{i}")
}

fn extension_in(path: &Path, set: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| set.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

/// The first file in `names` (a folder's children) whose stem, case-insensitively, is one of
/// `stems` in that order of preference, with an image extension.
fn pick_art(dir: &Path, names: &[String], stems: &[String]) -> Option<PathBuf> {
    for stem in stems {
        for ext in ART_EXTENSIONS {
            let want = format!("{stem}.{ext}");
            if let Some(found) = names.iter().find(|n| n.eq_ignore_ascii_case(&want)) {
                return Some(dir.join(found));
            }
        }
    }
    None
}

fn child_names(dir: &Path) -> std::io::Result<Vec<String>> {
    Ok(std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect())
}

fn folder_art(dir: &Path) -> Option<PathBuf> {
    let names = child_names(dir).ok()?;
    pick_art(dir, &names, &FOLDER_ART_NAMES.map(String::from))
}

/// What a subfolder shows as: its artwork, and — when it holds exactly one video and no folder
/// (the usual layout of a film: `Alien (1979)/Alien (1979).mkv` beside its poster) — that video,
/// which the folder's card then plays. Blocking; only a folder with one video is looked into
/// further than its names.
fn folder_entry(path: PathBuf, name: &str) -> Entry {
    let names = child_names(&path).unwrap_or_default();
    let folder_art = pick_art(&path, &names, &FOLDER_ART_NAMES.map(String::from));
    let mut videos = names.iter().filter(|n| !is_hidden(n) && extension_in(Path::new(n.as_str()), &VIDEO_EXTENSIONS));
    let lone = match (videos.next(), videos.next()) {
        (Some(video), None) => Some(video),
        _ => None,
    };
    let film = lone.and_then(|video| {
        let has_folder = names.iter().any(|n| !is_hidden(n) && path.join(n).is_dir());
        let meta = std::fs::metadata(path.join(video)).ok().filter(|m| m.is_file())?;
        (!has_folder).then(|| (video, meta.len()))
    });
    match film {
        Some((video, size)) => {
            let stem = Path::new(video.as_str()).file_stem().map_or_else(|| video.clone(), |s| s.to_string_lossy().into_owned());
            let art = folder_art.or_else(|| pick_art(&path, &names, &[format!("{stem}-poster"), stem]));
            Entry { title: name.to_owned(), path: path.join(video), kind: EntryKind::Video, art: art.map(|p| art_token(&p)), size }
        }
        None => Entry { title: name.to_owned(), path, kind: EntryKind::Folder, art: folder_art.map(|p| art_token(&p)), size: 0 },
    }
}

/// Read one folder: its subfolders and videos, sorted folders first then by natural order, with
/// their artwork resolved. A subfolder holding a single video is listed as that video
/// ([`folder_entry`]). Blocking.
pub fn scan(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let names = child_names(dir)?;
    let videos: Vec<&String> = names
        .iter()
        .filter(|n| !is_hidden(n) && extension_in(Path::new(n.as_str()), &VIDEO_EXTENSIONS) && dir.join(n).is_file())
        .collect();
    let lone_video_art = if videos.len() == 1 {
        pick_art(dir, &names, &FOLDER_ART_NAMES.map(String::from))
    } else {
        None
    };
    let mut entries = Vec::new();
    for name in &names {
        if is_hidden(name) {
            continue;
        }
        let path = dir.join(name);
        // Follows symlinks: a library assembled from links is a common layout.
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if meta.is_dir() {
            entries.push(folder_entry(path, name));
        } else if meta.is_file() && extension_in(&path, &VIDEO_EXTENSIONS) {
            let stem = Path::new(name).file_stem().map_or_else(|| name.clone(), |s| s.to_string_lossy().into_owned());
            let art = pick_art(dir, &names, &[format!("{stem}-poster"), stem.clone()])
                .or_else(|| lone_video_art.clone())
                .map(|p| art_token(&p));
            entries.push(Entry { title: stem, path, kind: EntryKind::Video, art, size: meta.len() });
        }
    }
    entries.sort_by(|a, b| {
        (a.kind != EntryKind::Folder)
            .cmp(&(b.kind != EntryKind::Folder))
            .then_with(|| natural_cmp(&a.title, &b.title))
    });
    Ok(entries)
}

/// A subtitle file beside a video ([`sidecars`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarFile {
    pub path: PathBuf,
    /// The format, as its extension names it: `srt`, `ass`, `ssa` or `vtt`.
    pub codec: String,
    /// The language its name carries (`Film.en.srt`, `Film.eng.srt`), as written; empty when none.
    pub lang: String,
    pub forced: bool,
    pub sdh: bool,
    /// The rest of its name between the video's and the extension (`Film.Director's cut.srt`).
    pub title: String,
}

const SIDECAR_EXTENSIONS: [&str; 4] = ["srt", "ass", "ssa", "vtt"];

/// The subtitle files beside `video`, by the naming every media player reads: the video's own
/// name, then any of a language (`en`, `eng`), `forced`, `sdh` / `cc` / `hi`, and free words, then
/// a text-subtitle extension. Blocking (it lists the folder); sorted by name.
pub fn sidecars(video: &Path) -> Vec<SidecarFile> {
    let (Some(dir), Some(stem)) = (video.parent(), video.file_stem().and_then(|s| s.to_str())) else {
        return Vec::new();
    };
    let mut names = child_names(dir).unwrap_or_default();
    names.sort_by(|a, b| natural_cmp(a, b));
    names.iter().filter_map(|name| sidecar_of(stem, name).map(|mut s| { s.path = dir.join(name); s })).collect()
}

/// Whether `name` is a subtitle for the video named `stem`, and what its name says about it.
fn sidecar_of(stem: &str, name: &str) -> Option<SidecarFile> {
    let rest = name.strip_prefix(stem)?;
    let (middle, ext) = rest.rsplit_once('.')?;
    let codec = ext.to_ascii_lowercase();
    if !SIDECAR_EXTENSIONS.contains(&codec.as_str()) || !(middle.is_empty() || middle.starts_with('.')) {
        return None;
    }
    let mut out = SidecarFile { path: PathBuf::new(), codec, lang: String::new(), forced: false, sdh: false, title: String::new() };
    let mut words = Vec::new();
    for token in middle.split('.').filter(|t| !t.is_empty()) {
        match token.to_ascii_lowercase().as_str() {
            "forced" => out.forced = true,
            "sdh" | "cc" | "hi" => out.sdh = true,
            "default" => {}
            t if out.lang.is_empty() && is_language(t) => out.lang = token.to_string(),
            _ => words.push(token),
        }
    }
    out.title = words.join(" ");
    Some(out)
}

/// A two-letter code, or a three-letter one the language table knows (`eng`, `ger`, `deu`).
fn is_language(token: &str) -> bool {
    let alpha = token.bytes().all(|b| b.is_ascii_alphabetic());
    alpha && (token.len() == 2 || (token.len() == 3 && crate::metadata::two_letter_code(token).is_some_and(|c| c != token)))
}

/// Case-insensitive order in which a run of digits compares by its value, so `Episode 2` sorts
/// before `Episode 10`.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        s.push(c);
                        it.next();
                    }
                    s
                };
                let (m, n) = (take(&mut x), take(&mut y));
                let (m, n) = (m.trim_start_matches('0'), n.trim_start_matches('0'));
                let ord = m.len().cmp(&n.len()).then_with(|| m.cmp(n));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(c), Some(d)) => {
                let ord = c.to_lowercase().cmp(d.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                x.next();
                y.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_holding_one_video_is_that_video() {
        let root = plx_base::testscratch::process_dir("local-film-folders");
        let film = root.join("Alien (1979)");
        let show = root.join("Show A");
        std::fs::create_dir_all(&film).unwrap();
        std::fs::create_dir_all(show.join("Season 1")).unwrap();
        std::fs::write(film.join("Alien.1979.1080p.mkv"), [0u8; 3]).unwrap();
        std::fs::write(film.join("poster.jpg"), [0u8; 1]).unwrap();
        std::fs::write(film.join("Alien.1979.1080p.en.srt"), [0u8; 1]).unwrap();
        std::fs::write(show.join("Trailer.mkv"), [0u8; 1]).unwrap();
        let entries = scan(&root).unwrap();
        let alien = entries.iter().find(|e| e.title == "Alien (1979)").unwrap();
        assert_eq!(alien.kind, EntryKind::Video, "one video and no folder: the film itself");
        assert_eq!(alien.path, film.join("Alien.1979.1080p.mkv"));
        assert_eq!(alien.size, 3);
        assert!(alien.art.is_some(), "it keeps the folder's poster");
        let show_entry = entries.iter().find(|e| e.title == "Show A").unwrap();
        assert_eq!(show_entry.kind, EntryKind::Folder, "a folder with a subfolder stays a folder, however few videos");
    }

    #[test]
    fn a_subtitle_file_is_read_by_its_name() {
        let f = |name| sidecar_of("Alien (1979)", name);
        assert_eq!(f("Alien (1979).srt").map(|s| (s.codec, s.lang)), Some(("srt".into(), String::new())));
        let en = f("Alien (1979).en.forced.ass").unwrap();
        assert_eq!((en.codec.as_str(), en.lang.as_str(), en.forced, en.sdh), ("ass", "en", true, false));
        let de = f("Alien (1979).ger.SDH.srt").unwrap();
        assert_eq!((de.lang.as_str(), de.sdh), ("ger", true));
        assert_eq!(f("Alien (1979).Commentary.srt").unwrap().title, "Commentary");
        assert_eq!(f("Alien (1979).mkv"), None, "not a subtitle");
        assert_eq!(f("Alien (1979) 2.srt"), None, "another video's subtitle");
        assert_eq!(f("Alien (1979).sup"), None, "an image subtitle is not offered");
        assert_eq!(f("Alien (1979).the.srt").unwrap().lang, "", "a word is not a language");
    }

    fn tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("plx-local-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn numbers_sort_by_value_and_case_is_ignored() {
        let mut v = vec!["Episode 10", "episode 2", "Episode 1", "alpha", "Beta"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["alpha", "Beta", "Episode 1", "episode 2", "Episode 10"]);
    }

    #[test]
    fn a_folder_lists_folders_first_then_videos_and_skips_the_rest() {
        let root = tree("list");
        touch(&root.join("Zulu (2001).mkv"));
        touch(&root.join("alpha.mp4"));
        touch(&root.join("notes.txt"));
        touch(&root.join(".hidden.mkv"));
        touch(&root.join("Shows/Show A/s01e01.mkv"));
        let e = scan(&root).unwrap();
        let titles: Vec<_> = e.iter().map(|e| (e.title.as_str(), e.kind)).collect();
        assert_eq!(titles, [("Shows", EntryKind::Folder), ("alpha", EntryKind::Video), ("Zulu (2001)", EntryKind::Video)]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn artwork_follows_the_media_folder_conventions() {
        let root = tree("art");
        touch(&root.join("Alien (1979)/Alien (1979).mkv"));
        touch(&root.join("Alien (1979)/Poster.JPG"));
        touch(&root.join("Mixed/one.mkv"));
        touch(&root.join("Mixed/one-poster.png"));
        touch(&root.join("Mixed/two.mkv"));
        touch(&root.join("Mixed/folder.jpg"));
        let top = scan(&root).unwrap();
        let alien = top.iter().find(|e| e.title == "Alien (1979)").unwrap();
        assert_eq!(art_path(alien.art.as_deref().unwrap()).unwrap(), root.join("Alien (1979)/Poster.JPG"));
        // The lone film in its folder wears the folder's poster.
        let inner = scan(&root.join("Alien (1979)")).unwrap();
        assert_eq!(art_path(inner[0].art.as_deref().unwrap()).unwrap(), root.join("Alien (1979)/Poster.JPG"));
        // With two films, each wears only its own; the folder's picture is the folder's.
        let mixed = scan(&root.join("Mixed")).unwrap();
        assert_eq!(art_path(mixed[0].art.as_deref().unwrap()).unwrap(), root.join("Mixed/one-poster.png"));
        assert_eq!(mixed[1].art, None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn roots_come_from_the_environment_or_default_to_videos() {
        assert_eq!(roots_from(Some("/a:/b:relative".into()), Some("/home/u".into())), [PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(roots_from(None, Some("/home/u".into())), [PathBuf::from("/home/u/Videos")]);
        assert_eq!(roots_from(Some("".into()), Some("/home/u".into())), [PathBuf::from("/home/u/Videos")]);
    }
}
