//! **Playing a file on this machine's own disk**, with no server behind it.
//!
//! A Plex playback learns what it is about to play from the server's decision: the codecs the
//! Load payload declares, the frame rate, the source raster. A local file has no server, so the
//! file is asked instead ([`crate::ff::probe_local`]) and the same declaration is installed on the
//! [`PlaybackSession`](crate::route::PlaybackSession) the engine reads, together with the file's
//! `file://` URL ([`crate::localfile`]). From there the playback is the ordinary engine path:
//! `start_bufferfeed` finds a URL on the route and a declaration to Load it with.
//!
//! Refusals are made here, before any Load, and say why in words a viewer can act on.

use std::path::Path;

/// The video codecs the Load payload can declare (`engine`'s `LoadVideo`), FFmpeg's name first.
const VIDEO_CODECS: [(&str, &str); 4] = [("h264", "H264"), ("hevc", "H265"), ("av1", "AV1"), ("vp9", "VP9")];

/// The Load payload's name for an FFmpeg video codec name.
fn payload_video(codec: &str) -> Option<&'static str> {
    VIDEO_CODECS.iter().find(|(f, _)| *f == codec).map(|(_, p)| *p)
}

#[derive(Debug, Clone, PartialEq)]
pub enum LocalPlayError {
    /// The file could not be opened or read as media.
    Unreadable(String),
    /// Its video is in a codec the player does not decode.
    VideoCodec(String),
    /// It has audio, and none of its tracks is in a codec the player carries; lists them.
    AudioCodecs(Vec<String>),
}

impl std::fmt::Display for LocalPlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LocalPlayError::Unreadable(why) => write!(f, "This file can't be played ({why})."),
            LocalPlayError::VideoCodec(c) => write!(f, "{} video isn't supported.", c.to_uppercase()),
            LocalPlayError::AudioCodecs(cs) => write!(
                f,
                "Its audio ({}) isn't supported yet.",
                cs.iter().map(|c| c.to_uppercase()).collect::<Vec<_>>().join(", ")
            ),
        }
    }
}

impl LocalPlayError {
    /// What the viewer is told, in the interface language. The [`Display`](std::fmt::Display)
    /// form is the event log's, and keeps FFmpeg's own reason for an unreadable file.
    pub fn message(&self) -> String {
        use plx_platform::i18n::msg;
        let upper = |c: &String| c.to_uppercase();
        match self {
            LocalPlayError::Unreadable(_) => msg::browse_local_unplayable().to_owned(),
            LocalPlayError::VideoCodec(c) => msg::browse_local_video_unsupported(&upper(c)),
            LocalPlayError::AudioCodecs(cs) => {
                msg::browse_local_audio_unsupported(&cs.iter().map(upper).collect::<Vec<_>>().join(", "))
            }
        }
    }
}

/// The checks a probe must pass before a Load is attempted. Pure, so the host suite pins them.
pub fn admit(probe: &crate::ff::LocalProbe) -> Result<(), LocalPlayError> {
    if payload_video(&probe.vcodec).is_none() {
        return Err(LocalPlayError::VideoCodec(probe.vcodec.clone()));
    }
    // A file with no audio at all plays silent; one whose every track is unplayable does not,
    // since its sound is the point of it.
    if probe.acodec.is_empty() && !probe.audio_codecs.is_empty() {
        return Err(LocalPlayError::AudioCodecs(probe.audio_codecs.clone()));
    }
    Ok(())
}

/// The title a file plays under: its name without the extension.
pub fn title_of(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// Probe `path` and, if it can play, install it on `ps` as the next thing the engine Loads.
/// Blocking (it opens and reads the file's headers); the caller decides which thread pays.
pub fn prepare(ps: &mut crate::route::PlaybackSession, path: &Path) -> Result<crate::ff::LocalProbe, LocalPlayError> {
    let probe = crate::ff::probe_local(path).map_err(LocalPlayError::Unreadable)?;
    // The Linux build asks this machine's GStreamer, so an AV1 or VP9 file on a machine without
    // that decoder is refused here, with a reason, rather than by a Load that never shows a frame.
    #[cfg(feature = "gst")]
    if let Some(name) = payload_video(&probe.vcodec) {
        if !crate::player::gst_sink::decodes_video(name) {
            return Err(LocalPlayError::VideoCodec(probe.vcodec.clone()));
        }
    }
    install(ps, path, &probe)?;
    Ok(probe)
}

/// Install an already-probed file on `ps`: the declaration, the source raster, the HUD title and
/// the `file://` URL — in that order, so a refused declaration never leaves a URL behind for a
/// later Play to pick up without it.
pub fn install(ps: &mut crate::route::PlaybackSession, path: &Path, probe: &crate::ff::LocalProbe) -> Result<(), LocalPlayError> {
    admit(probe)?;
    if !crate::route::set_stream_declaration(ps, &probe.vcodec, &probe.acodec, probe.fps, plx_data::metadata::Dovi::NONE, false) {
        return Err(LocalPlayError::Unreadable("the declaration was refused".into()));
    }
    if probe.width > 0 && probe.height > 0 {
        crate::route::set_stream_source_raster(ps, probe.width, probe.height);
    }
    let folder = path
        .parent()
        .and_then(|p| p.file_name())
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    crate::route::set_hud_title(ps, &title_of(path), &folder);
    // No account to say which subtitles are the viewer's: the interface's language leads the menu.
    crate::route::set_sub_pref_lang(ps, plx_platform::i18n::current().language().tag());
    crate::route::set_url(ps, &crate::localfile::file_url(path));
    crate::player::log(&format!(
        "localplay: {} v={} {}x{} fps={:.3} a={} (tracks: {})",
        title_of(path),
        probe.vcodec,
        probe.width,
        probe.height,
        probe.fps,
        probe.acodec,
        probe.audio_codecs.join(",")
    ));
    Ok(())
}

/// The track store the player's menus read for a local file (`metadata::PlayingItem`, which a
/// Plex playback fills from the server): its embedded subtitle tracks, in file order, then the
/// subtitle files beside it ([`plx_data::local::sidecars`]). The item's server is
/// `local::ART_SRV` and its key the file's path.
///
/// Its audio tracks carry FFmpeg's codec names, which is what the Load payload and the route's
/// direct-play test read on this build (`route::plan::audio_direct_plays`).
pub fn playing_item(path: &Path, probe: &crate::ff::LocalProbe, sidecars: &[plx_data::local::SidecarFile]) -> plx_data::metadata::PlayingItem {
    use plx_data::metadata::Stream;
    let named = |tag: &str| {
        plx_data::metadata::two_letter_code(tag).map_or_else(String::new, |c| plx_plex::plex::languages::label(&c).to_owned())
    };
    let audio = probe
        .audio
        .iter()
        .enumerate()
        .map(|(i, t)| Stream {
            id: AUDIO_ID + i as i64,
            index: i as i64,
            lang: named(&t.lang),
            lang_code: t.lang.clone(),
            codec: t.codec.clone(),
            channels: i64::from(t.channels),
            title: t.title.clone(),
            default: t.default,
            ..Stream::default()
        })
        .collect();
    let mut subs: Vec<Stream> = probe
        .subs
        .iter()
        .enumerate()
        .map(|(i, t)| Stream {
            id: EMBEDDED_SUB_ID + i as i64,
            index: i as i64,
            lang: named(&t.lang),
            lang_code: t.lang.clone(),
            codec: sub_codec(&t.codec).to_owned(),
            title: t.title.clone(),
            sdh: t.sdh,
            forced: t.forced,
            default: t.default,
            ..Stream::default()
        })
        .collect();
    subs.extend(sidecars.iter().enumerate().map(|(j, f)| Stream {
        id: SIDECAR_ID + j as i64,
        index: SIDECAR_ID + j as i64,
        lang: named(&f.lang),
        lang_code: f.lang.clone(),
        codec: f.codec.clone(),
        title: f.title.clone(),
        sdh: f.sdh,
        forced: f.forced,
        external: true,
        key: f.path.to_string_lossy().into_owned(),
        ..Stream::default()
    }));
    plx_data::metadata::PlayingItem {
        sid: plx_plex::plex::ServerId::from_raw(plx_data::local::ART_SRV),
        rk: path.to_string_lossy().into_owned(),
        show_rk: String::new(),
        kind: "movie".into(),
        audio,
        subs,
        video_fps: probe.fps,
        width: i64::from(probe.width),
        height: i64::from(probe.height),
        bitrate: 0,
        dovi: Default::default(),
        markers: Vec::new(),
        chapters: Vec::new(),
        blur: None,
    }
}

/// The track a local file starts on: the first in file order the sink decodes (the probe's
/// `acodec`, which the demuxer feeds by default), as the route records a pick.
pub fn starting_audio(item: &plx_data::metadata::PlayingItem, probe: &crate::ff::LocalProbe) -> Option<crate::route::CarriedAudio> {
    let i = item.audio.iter().position(|s| s.codec == probe.acodec)?;
    Some(crate::route::CarriedAudio::from_stream(&item.audio[i], plx_data::metadata::audio_ordinal(&item.audio, i)))
}

/// Audio stream ids for a local file, from 500 (any non-zero id names a track).
const AUDIO_ID: i64 = 500;

/// Stream ids for a local file's subtitles: embedded tracks from 1, files beside it from 1000.
/// Any non-zero id will do (0 is Off); the two ranges keep a file's sidecar from ever sharing an
/// embedded track's id.
const EMBEDDED_SUB_ID: i64 = 1;
const SIDECAR_ID: i64 = 1000;

/// FFmpeg's subtitle codec name in the spelling the track menu reads (PMS's).
fn sub_codec(ffmpeg: &str) -> &str {
    match ffmpeg {
        "subrip" => "srt",
        "webvtt" => "vtt",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(v: &str, a: &str, all: &[&str]) -> crate::ff::LocalProbe {
        crate::ff::LocalProbe {
            vcodec: v.into(),
            acodec: a.into(),
            audio_codecs: all.iter().map(|s| (*s).into()).collect(),
            audio: Vec::new(),
            subs: Vec::new(),
            width: 1920,
            height: 1080,
            fps: 23.976,
            duration_ns: 0,
        }
    }

    #[test]
    fn the_track_store_lists_embedded_subtitles_then_the_files_beside_it() {
        let mut p = probe("hevc", "eac3", &["eac3"]);
        p.subs = vec![crate::ff::LocalTrack { codec: "subrip".into(), lang: "eng".into(), forced: true, ..Default::default() }];
        let side = plx_data::local::SidecarFile {
            path: "/m/Film.de.srt".into(), codec: "srt".into(), lang: "de".into(), forced: false, sdh: true, title: String::new(),
        };
        let item = playing_item(Path::new("/m/Film.mkv"), &p, &[side]);
        assert_eq!(item.rk, "/m/Film.mkv");
        assert_eq!(item.sid.raw(), plx_data::local::ART_SRV);
        assert_eq!(item.audio.len(), 0, "the fixture's probe lists no audio tracks");
        let (embedded, file) = (&item.subs[0], &item.subs[1]);
        assert_eq!((embedded.codec.as_str(), embedded.lang_code.as_str(), embedded.forced, embedded.external), ("srt", "eng", true, false));
        assert!(!embedded.lang.is_empty(), "a tagged track is named");
        assert!(file.external && file.sidecar_renderable() && file.sdh);
        assert_eq!(file.key, "/m/Film.de.srt");
        assert_ne!(embedded.id, file.id);
        assert_eq!(plx_data::metadata::sub_render_ordinal(&item.subs, 0), 0, "the embedded track is the demuxer's first");
        assert_eq!(plx_data::metadata::sub_render_ordinal(&item.subs, 1), -1, "a file beside it is not the demuxer's");
    }

    #[test]
    fn the_file_starts_on_its_first_decodable_audio_track() {
        let mut p = probe("hevc", "ac3", &["truehd", "ac3"]);
        let track = |codec: &str| crate::ff::LocalTrack { codec: codec.into(), channels: 6, ..Default::default() };
        p.audio = vec![track("truehd"), track("ac3")];
        let item = playing_item(Path::new("/m/Film.mkv"), &p, &[]);
        assert_eq!(item.audio.iter().map(|s| s.codec.as_str()).collect::<Vec<_>>(), ["truehd", "ac3"]);
        let start = starting_audio(&item, &p).unwrap();
        assert_eq!((start.sid, start.ordinal, start.codec.as_str()), (item.audio[1].id, 1, "ac3"));
    }

    #[test]
    fn a_playable_file_is_admitted() {
        assert_eq!(admit(&probe("hevc", "eac3", &["truehd", "eac3"])), Ok(()));
        assert_eq!(admit(&probe("h264", "aac", &["aac"])), Ok(()));
    }

    #[test]
    fn the_viewer_is_told_in_the_catalog_s_words() {
        assert_eq!(LocalPlayError::Unreadable("avformat_open_input -1094995529".into()).message(), "This file can’t be played.");
        assert_eq!(LocalPlayError::VideoCodec("vc1".into()).message(), "VC1 video can’t be played here.");
        assert_eq!(
            LocalPlayError::AudioCodecs(vec!["cook".into(), "wmav2".into()]).message(),
            "Its audio (COOK, WMAV2) can’t be played here."
        );
    }

    #[test]
    fn refusals_name_what_is_wrong() {
        let e = admit(&probe("mpeg2video", "ac3", &["ac3"])).unwrap_err();
        assert_eq!(e.to_string(), "MPEG2VIDEO video isn't supported.");
        let e = admit(&probe("hevc", "", &["cook", "wmav2"])).unwrap_err();
        assert_eq!(e.to_string(), "Its audio (COOK, WMAV2) isn't supported yet.");
        assert_eq!(admit(&probe("hevc", "", &[])), Ok(()), "a silent file plays");
    }

    #[test]
    fn the_title_is_the_file_name_without_its_extension() {
        assert_eq!(title_of(Path::new("/m/Films/Alien (1979).mkv")), "Alien (1979)");
        assert_eq!(title_of(Path::new("/m/noext")), "noext");
    }
}
