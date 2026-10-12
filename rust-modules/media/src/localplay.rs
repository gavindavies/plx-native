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

/// The video codecs the Load payload can declare (`engine`'s `LoadVideo`).
const VIDEO_CODECS: [&str; 4] = ["h264", "hevc", "av1", "vp9"];

#[derive(Debug, Clone, PartialEq)]
pub enum LocalPlayError {
    /// The file could not be opened or read as media.
    Unreadable(String),
    /// Its video is in a codec the player does not decode.
    VideoCodec(String),
    /// None of its audio tracks is in a codec the player carries; lists what it has.
    AudioCodecs(Vec<String>),
}

impl std::fmt::Display for LocalPlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LocalPlayError::Unreadable(why) => write!(f, "This file can't be played ({why})."),
            LocalPlayError::VideoCodec(c) => write!(f, "{} video isn't supported.", c.to_uppercase()),
            LocalPlayError::AudioCodecs(cs) if cs.is_empty() => f.write_str("This file has no audio track."),
            LocalPlayError::AudioCodecs(cs) => write!(
                f,
                "Its audio ({}) isn't supported yet.",
                cs.iter().map(|c| c.to_uppercase()).collect::<Vec<_>>().join(", ")
            ),
        }
    }
}

/// The checks a probe must pass before a Load is attempted. Pure, so the host suite pins them.
pub fn admit(probe: &crate::ff::LocalProbe) -> Result<(), LocalPlayError> {
    if !VIDEO_CODECS.contains(&probe.vcodec.as_str()) {
        return Err(LocalPlayError::VideoCodec(probe.vcodec.clone()));
    }
    if probe.acodec.is_empty() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(v: &str, a: &str, all: &[&str]) -> crate::ff::LocalProbe {
        crate::ff::LocalProbe {
            vcodec: v.into(),
            acodec: a.into(),
            audio_codecs: all.iter().map(|s| (*s).into()).collect(),
            width: 1920,
            height: 1080,
            fps: 23.976,
            duration_ns: 0,
        }
    }

    #[test]
    fn a_playable_file_is_admitted() {
        assert_eq!(admit(&probe("hevc", "eac3", &["truehd", "eac3"])), Ok(()));
        assert_eq!(admit(&probe("h264", "aac", &["aac"])), Ok(()));
    }

    #[test]
    fn refusals_name_what_is_wrong() {
        let e = admit(&probe("mpeg2video", "ac3", &["ac3"])).unwrap_err();
        assert_eq!(e.to_string(), "MPEG2VIDEO video isn't supported.");
        let e = admit(&probe("hevc", "", &["truehd", "flac"])).unwrap_err();
        assert_eq!(e.to_string(), "Its audio (TRUEHD, FLAC) isn't supported yet.");
        let e = admit(&probe("hevc", "", &[])).unwrap_err();
        assert_eq!(e.to_string(), "This file has no audio track.");
    }

    #[test]
    fn the_title_is_the_file_name_without_its_extension() {
        assert_eq!(title_of(Path::new("/m/Films/Alien (1979).mkv")), "Alien (1979)");
        assert_eq!(title_of(Path::new("/m/noext")), "noext");
    }
}
