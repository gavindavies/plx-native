//! **What a title says about itself, read from the Kodi-style files beside it.**
//!
//! The local library has no scraper and no index (`local`'s module docs): a title is whatever its
//! folder holds. Media folders already carry their descriptions in a convention every media centre
//! reads and writes — Kodi's — so the library reads that and nothing else:
//!
//! - **`.nfo`**: an XML file beside the video, `<stem>.nfo` (or `movie.nfo` in a film's own folder),
//!   and `tvshow.nfo` inside a show's folder. [`parse_nfo`] reads the fields the screens show: the
//!   title, year, runtime, genres, plot, tagline, directors, the collection (`<set>`) and, from
//!   `<fileinfo><streamdetails>`, the main audio's channel count. An `.nfo` that is only a scraper
//!   URL (Kodi allows that) is not a description and reads as `None`.
//! - **A backdrop**: `<stem>-fanart.*`, then `fanart.*`, `backdrop.*` or `background.*`.
//! - **A title logo**: `<stem>-clearlogo.png`, then `clearlogo.png` or `logo.png` — PNG only,
//!   because a logo is drawn over artwork and needs its alpha.
//!
//! Every field is optional: a title with no `.nfo` is shown by its file name, as it always was.
//! Everything here is pure over a folder's child names except [`read`], which reads one file and is
//! called from `local::scan`, off the frame.

use std::path::{Path, PathBuf};

/// A title's description, as its `.nfo` gives it. Every field may be empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    /// What the `.nfo`'s root element says this is.
    pub kind: MetaKind,
    /// `<title>`; empty when the `.nfo` has none (the file name then names the title).
    pub title: String,
    pub year: Option<u16>,
    /// `<runtime>`, in minutes.
    pub runtime_min: Option<u32>,
    pub genres: Vec<String>,
    /// `<plot>`, else `<outline>`.
    pub plot: String,
    pub tagline: String,
    pub directors: Vec<String>,
    /// `<set><name>` (or a bare `<set>`): the collection the title belongs to.
    pub collection: String,
    /// The first `<streamdetails><audio><channels>`.
    pub audio_channels: Option<u8>,
    /// `<streamdetails>` lists a `<subtitle>`.
    pub has_subtitles: bool,
}

/// The root element of an `.nfo`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MetaKind {
    #[default]
    Movie,
    Show,
    Episode,
}

/// The `.nfo` file names that describe the video named `stem` in a folder holding `names`, in the
/// order they are tried. `movie.nfo` describes a film only when it is the folder's one video
/// (`lone`), the one-film-per-folder layout.
pub fn nfo_candidates(stem: &str, lone: bool) -> Vec<String> {
    let mut out = vec![format!("{stem}.nfo")];
    if lone {
        out.push("movie.nfo".to_owned());
    }
    out
}

/// The backdrop picture for `stem` among a folder's `names` (`stem` empty: the folder's own).
pub fn backdrop_in(dir: &Path, names: &[String], stem: &str) -> Option<PathBuf> {
    let mut stems = Vec::new();
    if !stem.is_empty() {
        stems.push(format!("{stem}-fanart"));
    }
    stems.extend(["fanart", "backdrop", "background"].map(String::from));
    pick(dir, names, &stems, &["jpg", "jpeg", "png"])
}

/// The title logo for `stem` among a folder's `names` (`stem` empty: the folder's own).
pub fn logo_in(dir: &Path, names: &[String], stem: &str) -> Option<PathBuf> {
    let mut stems = Vec::new();
    if !stem.is_empty() {
        stems.push(format!("{stem}-clearlogo"));
    }
    stems.extend(["clearlogo", "logo"].map(String::from));
    pick(dir, names, &stems, &["png"])
}

/// The first of `names` matching one of `stems` (in order of preference) with one of `exts`,
/// case-insensitively.
fn pick(dir: &Path, names: &[String], stems: &[String], exts: &[&str]) -> Option<PathBuf> {
    for stem in stems {
        for ext in exts {
            let want = format!("{stem}.{ext}");
            if let Some(found) = names.iter().find(|n| n.eq_ignore_ascii_case(&want)) {
                return Some(dir.join(found));
            }
        }
    }
    None
}

/// Read the first of `candidates` (file names in `dir`, matched case-insensitively against
/// `names`) that parses as a description. Blocking.
pub fn read(dir: &Path, names: &[String], candidates: &[String]) -> Option<Meta> {
    candidates.iter().find_map(|want| {
        let found = names.iter().find(|n| n.eq_ignore_ascii_case(want))?;
        let bytes = std::fs::read(dir.join(found)).ok()?;
        parse_nfo(&String::from_utf8_lossy(&bytes))
    })
}

/// The description an `.nfo`'s text holds, or `None` when it has no `<movie>`, `<tvshow>` or
/// `<episodedetails>` element (a URL-only `.nfo`, or not XML at all).
pub fn parse_nfo(text: &str) -> Option<Meta> {
    let (kind, body) = [("movie", MetaKind::Movie), ("tvshow", MetaKind::Show), ("episodedetails", MetaKind::Episode)]
        .into_iter()
        .find_map(|(tag, kind)| elements(text, tag).into_iter().next().map(|b| (kind, b)))?;
    // The fields read are direct facts of the title; `<fileinfo>` and `<actor>` blocks nest their
    // own `<name>`/`<title>`-like children, so they are cut out before the plain fields are read.
    let stream = elements(body, "streamdetails").into_iter().next().unwrap_or("");
    let plain = without(&without(body, "fileinfo"), "actor");
    let one = |tag: &str| elements(&plain, tag).into_iter().map(text_of).find(|s| !s.is_empty()).unwrap_or_default();
    let many = |tag: &str| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for v in elements(&plain, tag).into_iter().map(text_of) {
            // Some writers put several values in one element, separated by " / ".
            for part in v.split(" / ").map(str::trim).filter(|s| !s.is_empty()) {
                if !out.iter().any(|o| o == part) {
                    out.push(part.to_owned());
                }
            }
        }
        out
    };
    let set = elements(&plain, "set").into_iter().next().map_or_else(String::new, |s| {
        elements(s, "name").into_iter().next().map_or_else(|| text_of(s), text_of)
    });
    let audio = elements(stream, "audio").into_iter().next().unwrap_or("");
    Some(Meta {
        kind,
        title: one("title"),
        year: one("year").parse().ok().filter(|y| *y > 0).or_else(|| {
            let date = one("premiered");
            date.get(..4).and_then(|y| y.parse().ok())
        }),
        runtime_min: one("runtime").split_whitespace().next().and_then(|r| r.parse().ok()).filter(|m| *m > 0),
        genres: many("genre"),
        plot: { let p = one("plot"); if p.is_empty() { one("outline") } else { p } },
        tagline: one("tagline"),
        directors: many("director"),
        collection: set,
        audio_channels: elements(audio, "channels").into_iter().next().and_then(|c| text_of(c).parse().ok()),
        has_subtitles: !elements(stream, "subtitle").is_empty(),
    })
}

/// The inner text of every `<tag …>…</tag>` in `xml`, outermost first; a self-closed `<tag/>`
/// is an empty one. Tags are matched by exact name (`<set>` is not `<settings>`), case-sensitively
/// as XML is. Not a validating parser: what an `.nfo` holds is a flat list of simple elements.
fn elements<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    spans(xml, tag).into_iter().map(|(_, inner)| &xml[inner]).collect()
}

/// `xml` with every `<tag>…</tag>` element removed.
fn without(xml: &str, tag: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut at = 0;
    for (outer, _) in spans(xml, tag) {
        out.push_str(&xml[at..outer.start]);
        at = outer.end;
    }
    out.push_str(&xml[at..]);
    out
}

/// Each `<tag>` element's whole extent and its inner text's, in document order.
fn spans(xml: &str, tag: &str) -> Vec<(std::ops::Range<usize>, std::ops::Range<usize>)> {
    let mut out = Vec::new();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut at = 0;
    while let Some(i) = xml[at..].find(&open) {
        let start = at + i;
        let after = start + open.len();
        // The name must end here: `>`, `/` or whitespace.
        let Some(next) = xml[after..].chars().next() else { break };
        if !(next == '>' || next == '/' || next.is_whitespace()) {
            at = after;
            continue;
        }
        let Some(gt) = xml[after..].find('>') else { break };
        let head_end = after + gt;
        if xml[..head_end].ends_with('/') {
            out.push((start..head_end + 1, head_end..head_end));
            at = head_end + 1;
            continue;
        }
        let body = head_end + 1;
        let Some(end) = xml[body..].find(&close) else { break };
        out.push((start..body + end + close.len(), body..body + end));
        at = body + end + close.len();
    }
    out
}

/// An element's text: CDATA unwrapped, nested markup and comments dropped, entities decoded,
/// whitespace collapsed.
fn text_of(inner: &str) -> String {
    let mut raw = String::with_capacity(inner.len());
    let mut rest = inner;
    while !rest.is_empty() {
        if let Some(cdata) = rest.strip_prefix("<![CDATA[") {
            let end = cdata.find("]]>").unwrap_or(cdata.len());
            raw.push_str(&cdata[..end].replace('&', "&amp;"));
            rest = cdata.get(end + 3..).unwrap_or("");
        } else if let Some(comment) = rest.strip_prefix("<!--") {
            rest = comment.find("-->").map_or("", |e| &comment[e + 3..]);
        } else if rest.starts_with('<') {
            rest = rest.find('>').map_or("", |e| &rest[e + 1..]);
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            raw.push_str(&rest[..end]);
            rest = &rest[end..];
        }
    }
    decode_entities(&raw).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest.find(';').filter(|&n| n <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..semi];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => name
                .strip_prefix("#x")
                .or_else(|| name.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| name.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHARGE: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes" ?>
<!-- written by build.py -->
<movie>
    <title>Charge</title>
    <originaltitle>Charge</originaltitle>
    <year>2022</year>
    <runtime>6</runtime>
    <plot>In a frozen, energy-starved future, an old man breaks into a guarded facility &amp; steals a battery.</plot>
    <tagline><![CDATA[One battery. One guard. <No> second chances.]]></tagline>
    <genre>Animation</genre>
    <genre>Action / Science Fiction</genre>
    <director>Hjalti Hjalmarsson</director>
    <set>
        <name>Blender Open Movies</name>
        <overview>Films made by Blender Studio.</overview>
    </set>
    <actor><name>Nobody</name><role>Guard</role></actor>
    <fileinfo>
        <streamdetails>
            <video><codec>h264</codec></video>
            <audio><codec>ac3</codec><language>eng</language><channels>6</channels></audio>
            <audio><codec>aac</codec><channels>2</channels></audio>
            <subtitle><language>eng</language></subtitle>
        </streamdetails>
    </fileinfo>
</movie>
"#;

    #[test]
    fn a_kodi_movie_nfo_gives_the_fields_the_screens_show() {
        let m = parse_nfo(CHARGE).unwrap();
        assert_eq!(m.kind, MetaKind::Movie);
        assert_eq!(m.title, "Charge");
        assert_eq!(m.year, Some(2022));
        assert_eq!(m.runtime_min, Some(6));
        assert_eq!(m.plot, "In a frozen, energy-starved future, an old man breaks into a guarded facility & steals a battery.");
        assert_eq!(m.tagline, "One battery. One guard. <No> second chances.", "CDATA is text, markup and all");
        assert_eq!(m.genres, ["Animation", "Action", "Science Fiction"], "one element per genre, or ' / '-joined");
        assert_eq!(m.directors, ["Hjalti Hjalmarsson"]);
        assert_eq!(m.collection, "Blender Open Movies", "the set's <name>, not its overview");
        assert_eq!(m.audio_channels, Some(6), "the FIRST audio stream's channels");
        assert!(m.has_subtitles);
    }

    #[test]
    fn a_bare_set_and_a_premiered_date_are_read_too() {
        let m = parse_nfo("<movie><title>Nosferatu</title><premiered>1922-03-04</premiered><set>Silent Classics</set>\
            <runtime>94 min</runtime><outline>A vampire.</outline></movie>").unwrap();
        assert_eq!(m.year, Some(1922));
        assert_eq!(m.collection, "Silent Classics");
        assert_eq!(m.runtime_min, Some(94), "a unit after the number is tolerated");
        assert_eq!(m.plot, "A vampire.", "the outline stands in for a missing plot");
        assert_eq!(m.audio_channels, None);
        assert!(!m.has_subtitles);
    }

    #[test]
    fn a_show_and_an_episode_say_what_they_are() {
        assert_eq!(parse_nfo("<tvshow><title>Caminandes</title></tvshow>").unwrap().kind, MetaKind::Show);
        assert_eq!(parse_nfo("<episodedetails><title>Gran Dillama</title></episodedetails>").unwrap().kind, MetaKind::Episode);
    }

    #[test]
    fn an_nfo_that_is_only_a_scraper_url_is_not_a_description() {
        assert_eq!(parse_nfo("https://www.themoviedb.org/movie/12345\n"), None);
        assert_eq!(parse_nfo(""), None);
    }

    #[test]
    fn a_tag_is_matched_by_its_whole_name() {
        let m = parse_nfo("<movie><settings>x</settings><title lang=\"en\">Spring</title><titles>no</titles><set/></movie>").unwrap();
        assert_eq!(m.title, "Spring", "attributes are skipped, <titles> is not <title>");
        assert_eq!(m.collection, "", "<settings> is not <set>, and an empty <set/> is no collection");
    }

    #[test]
    fn entities_decode_and_unknown_ones_stay_as_written() {
        assert_eq!(text_of("Tom &amp; Jerry &#233;&#xE9; &bogus; &"), "Tom & Jerry éé &bogus; &");
        assert_eq!(text_of("  two\n   lines <b>bold</b> <!-- gone --> "), "two lines bold");
    }

    #[test]
    fn artwork_beside_a_video_wins_over_the_folders() {
        let dir = Path::new("/m/Charge (2022)");
        let names: Vec<String> = ["Charge (2022).mkv", "Fanart.JPG", "Charge (2022)-fanart.png", "clearlogo.png", "logo.jpg"]
            .map(String::from)
            .to_vec();
        assert_eq!(backdrop_in(dir, &names, "Charge (2022)"), Some(dir.join("Charge (2022)-fanart.png")));
        assert_eq!(backdrop_in(dir, &names, ""), Some(dir.join("Fanart.JPG")));
        assert_eq!(logo_in(dir, &names, "Charge (2022)"), Some(dir.join("clearlogo.png")), "a JPEG logo has no alpha and is not one");
        assert_eq!(nfo_candidates("Charge (2022)", true), ["Charge (2022).nfo", "movie.nfo"]);
        assert_eq!(nfo_candidates("Episode 1", false), ["Episode 1.nfo"]);
    }
}
