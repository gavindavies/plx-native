//! **Follows a local file's playback and keeps its place** (`plx_data::local_progress`).
//!
//! A Plex playback reports its progress to the server (`/:/timeline`, the scrobble); a local file
//! has nobody to report to, so this records it instead. [`begin`] is called when a local file is
//! handed to the player; from then [`tick`] reads the player's position once a frame while the
//! player page is up, saves it every [`SAVE_EVERY_MS`] (so a crash or a power cut loses little),
//! and settles the record when the player page closes — with the player's own end-of-stream
//! ([`note_ended`]), so a film watched to the end is marked watched however long its credits are.
//!
//! It also keeps the viewer's **choices** for the file (`local_progress::Choices`): the audio
//! track, the subtitle and its timing are re-applied when the file is played again ([`play`]),
//! and recorded when the viewer changes one ([`tick`]).

use plx_data::local_progress;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// How often a running playback's place is saved.
const SAVE_EVERY_MS: u32 = 10_000;
/// A player that never opens (the Load was refused) is forgotten after this long.
const MOUNT_WAIT_MS: u32 = 20_000;

struct Watch {
    path: PathBuf,
    size: u64,
    /// The playing item's track ids, in the item's order: what a choice is translated through.
    audio_ids: Vec<i64>,
    subs: Vec<SubId>,
    /// `(audio, subtitle, offset)` as last seen, `None` until the first look: only a CHANGE from
    /// what the play started with is the viewer's choice.
    seen: Option<(i64, i64, i64)>,
    /// The frame time [`begin`] ran, until the player first shows; then the last save's.
    since: Option<u32>,
    mounted: bool,
    position_ms: u64,
    duration_ms: u64,
    ended: bool,
}

static WATCH: Mutex<Option<Watch>> = Mutex::new(None);

/// A local file (`size` bytes) has been handed to the player. Replaces any playback still being
/// followed, which is settled first, as its player is being replaced.
pub(crate) fn begin(path: PathBuf, size: u64, item: &plx_data::metadata::PlayingItem) {
    let mut watch = WATCH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(old) = watch.take() {
        settle(&old);
    }
    *watch = Some(Watch {
        path,
        size,
        audio_ids: item.audio.iter().map(|s| s.id).collect(),
        subs: sub_ids(&item.subs),
        seen: None,
        since: None,
        mounted: false,
        position_ms: 0,
        duration_ms: 0,
        ended: false,
    });
}

/// A subtitle track's id and what a choice calls it: an embedded track's position among the
/// embedded ones, or a file's name.
struct SubId {
    id: i64,
    choice: local_progress::SubtitleChoice,
}

fn sub_ids(subs: &[plx_data::metadata::Stream]) -> Vec<SubId> {
    let mut embedded = 0u32;
    subs.iter()
        .map(|s| {
            let choice = if s.external {
                let name = Path::new(&s.key).file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                local_progress::SubtitleChoice::File(name)
            } else {
                embedded += 1;
                local_progress::SubtitleChoice::Track(embedded - 1)
            };
            SubId { id: s.id, choice }
        })
        .collect()
}

/// What the session now plays, as a choice: the audio track's position, the subtitle and its timing.
fn choices_now(w: &Watch, audio_sid: i64, sub_sid: i64, offset_ms: i64) -> local_progress::Choices {
    local_progress::Choices {
        audio: w.audio_ids.iter().position(|&id| id == audio_sid).map(|i| i as u32),
        subtitle: Some(if sub_sid == 0 {
            local_progress::SubtitleChoice::Off
        } else {
            w.subs.iter().find(|s| s.id == sub_sid).map_or(local_progress::SubtitleChoice::Off, |s| s.choice.clone())
        }),
        subtitle_offset_ms: offset_ms,
        size: w.size,
    }
}

/// The audio track this play starts on: the viewer's choice for the file when it is one the sink
/// decodes, else the file's first decodable track. Staged BEFORE the play starts, because the
/// start builds the Load from it (`player::stage_native_audio`: its codec and the demuxer's index).
fn stage_audio(
    ps: &mut plx_media::playback::PlaybackSession,
    item: &plx_data::metadata::PlayingItem,
    probe: &plx_media::ff::LocalProbe,
    saved: Option<&local_progress::Choices>,
) -> Option<plx_media::playback::CarriedAudio> {
    let start = plx_media::localplay::starting_audio(item, probe);
    let audio = saved
        .and_then(|c| c.audio)
        .map(|i| i as usize)
        .filter(|&i| item.audio.get(i).is_some_and(|s| plx_media::player::engine::audio_payload_codec(&s.codec).is_some()))
        .map(|i| plx_media::playback::CarriedAudio::from_stream(&item.audio[i], plx_data::metadata::audio_ordinal(&item.audio, i)))
        .or(start);
    if let Some(audio) = &audio {
        plx_media::player::stage_native_audio(ps, audio.ordinal, &audio.codec);
    }
    audio
}

/// The subtitle this play shows: the viewer's choice for the file with its timing, else Off —
/// which also clears whatever the PREVIOUS local file had on (a new Plex item is reset by its
/// resolve; a local one has none). After the start, which leaves the subtitle alone.
fn apply_subtitle(
    ps: &mut plx_media::playback::PlaybackSession,
    item: &plx_data::metadata::PlayingItem,
    saved: Option<&local_progress::Choices>,
) {
    use plx_appkit::track_menu::TrackCommit;
    plx_media::player::reset_subtitle();
    let off = TrackCommit::Subtitle { render_ordinal: -1, stream_id: 0, sidecar_key: None, sidecar_codec: String::new() };
    super::playback::commit_track(ps, off);
    let Some(saved) = saved else { return };
    let ids = sub_ids(&item.subs);
    if let Some(i) = saved.subtitle.as_ref().and_then(|c| ids.iter().position(|s| &s.choice == c)) {
        let s = &item.subs[i];
        super::playback::commit_track(ps, TrackCommit::Subtitle {
            render_ordinal: plx_data::metadata::sub_render_ordinal(&item.subs, i),
            stream_id: s.id,
            sidecar_key: s.external.then(|| s.key.clone()),
            sidecar_codec: s.codec.clone(),
        });
        // After the pick: a new subtitle starts from no offset.
        plx_media::player::set_subtitle_offset(saved.subtitle_offset_ms);
    }
}

fn settle(w: &Watch) {
    if w.mounted && (w.position_ms > 0 || w.ended) {
        local_progress::record(&w.path, w.size, w.position_ms, w.duration_ms, w.ended);
        plx_base::eventlog::log(&format!(
            "local progress: {} stopped at {}s of {}s{}",
            w.path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            w.position_ms / 1_000,
            w.duration_ms / 1_000,
            if w.ended { " (end of stream)" } else { "" }
        ));
    }
}

/// Play the local file at `path`: resumed where it was left unless `from_start`. Probes it first
/// (a local disk answers in milliseconds, and the press that asked is this frame's only work); a
/// file that cannot play is refused on screen and nothing else happens. `ret` is where BACK out of
/// the player returns to.
pub(crate) fn play(
    ps: &mut plx_media::playback::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    path: &Path,
    from_start: bool,
    ret: Option<plx_ui::screen::ReturnState<u32, plx_screens::registry::PageMemory>>,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) {
    retire_orphan(ps, pa, super::bridge::player(pages).is_some());
    plx_media::playback::clear_url(ps);
    let probe = match plx_media::localplay::prepare(ps, path) {
        Ok(probe) => probe,
        Err(e) => {
            plx_base::eventlog::log(&format!("local play: refused: {e}"));
            super::toast::raise(e.message());
            return;
        }
    };
    // The probe just read this file, so the size is a cached stat away.
    let size = std::fs::metadata(path).map_or(0, |m| m.len());
    let resume_ms = if from_start { 0 } else { local_progress::resume_ms(path, size) };
    // The tracks the menus offer: the file's own and the subtitle files beside it.
    let item = plx_media::localplay::playing_item(path, &probe, &plx_data::local::sidecars(path));
    begin(path.to_path_buf(), size, &item);
    let saved = local_progress::choices(path, size);
    let audio = stage_audio(ps, &item, &probe, saved.as_ref());
    super::playback::start_playback(ps, pa, resume_ms.saturating_mul(1_000_000) as i64,
        super::playback::Origin::Here, plx_appkit::player_hud::LINGER_MS, ret, pages, bridge);
    // After the start, which retires the previous item's store as it begins.
    if let Some(audio) = audio {
        plx_media::playback::set_cur_audio(ps, audio);
    }
    apply_subtitle(ps, &item, saved.as_ref());
    bridge.metadata_mut().run(plx_data::stores::metadata::MetadataCmd::InstallPlaying(Some(item)));
}

/// A live engine that is not the player page's: every transport key and the EOS teardown are gated
/// on the player page, so nothing would ever stop it, and the next Load would meet it as "a live
/// engine that belongs to another Load attempt" and fail. Stop it before the new Load.
fn retire_orphan(
    ps: &mut plx_media::playback::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    player_mounted: bool,
) {
    if pa.is_live() && !player_mounted {
        plx_base::eventlog::log("play: stopping an off-route engine nothing owns before the new Load");
        plx_media::player::stop_bufferfeed(ps, pa);
    }
}

/// The item menu's rows: play the file from its start, or write its watch record.
pub(crate) fn apply_menu_action(
    ps: &mut plx_media::playback::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    act: plx_screens::item_menu::Action,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) {
    use plx_screens::item_menu::Action;
    let path = PathBuf::from(act.path());
    let size = || std::fs::metadata(&path).map_or(0, |m| m.len());
    match act {
        Action::PlayFromStart(_) => play(ps, pa, &path, true, None, pages, bridge),
        Action::MarkWatched(_) => local_progress::set_watched(&path, size(), true),
        Action::MarkUnwatched(_) => local_progress::set_watched(&path, size(), false),
    }
}

/// The player ran to the end of the stream. The frame loop says so as it closes the player,
/// which clears the player's own flag in the same frame, before [`tick`] could read it.
pub(crate) fn note_ended() {
    if let Some(w) = WATCH.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        w.ended = true;
    }
}

/// Once a frame.
pub(crate) fn tick(app: &super::App, now: u32) {
    let mut guard = WATCH.lock().unwrap_or_else(|e| e.into_inner());
    let Some(w) = guard.as_mut() else { return };
    // The viewer's choices: three integers a frame, and a record only when one moves.
    let ps = &app.player.session;
    let now_seen = (plx_media::playback::cur_audio_sid(ps), plx_media::playback::cur_sub_sid(ps), plx_media::player::subtitle_offset_ms());
    match w.seen {
        Some(seen) if seen != now_seen => {
            local_progress::set_choices(&w.path, choices_now(w, now_seen.0, now_seen.1, now_seen.2));
            w.seen = Some(now_seen);
        }
        Some(_) => {}
        None => w.seen = Some(now_seen),
    }
    let since = *w.since.get_or_insert(now);
    if super::bridge::player(&app.pages).is_none() {
        if w.mounted || now.wrapping_sub(since) >= MOUNT_WAIT_MS {
            // The player closed (or never opened): settle and stop following.
            if let Some(w) = guard.take() {
                settle(&w);
            }
        }
        return;
    }
    if !w.mounted {
        w.mounted = true;
        w.since = Some(now);
    }
    // Only a position the viewer has seen: before the first picture the clock is not this file's.
    if plx_media::player::seen_frame() {
        let pos = plx_media::player::playpos_ns();
        let dur = plx_media::player::duration_ns();
        if pos > 0 {
            w.position_ms = (pos / 1_000_000) as u64;
        }
        if dur > 0 {
            w.duration_ms = (dur / 1_000_000) as u64;
        }
    }
    if plx_media::player::ended() {
        w.ended = true;
    }
    if now.wrapping_sub(w.since.unwrap_or(now)) >= SAVE_EVERY_MS && w.position_ms > 0 {
        local_progress::record(&w.path, w.size, w.position_ms, w.duration_ms, w.ended);
        w.since = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_progress::SubtitleChoice;

    /// A choice names a track by what survives a rescan: an embedded track by its place among the
    /// embedded ones, a file beside the video by its name, the audio by its place.
    #[test]
    fn the_playing_tracks_read_back_as_the_viewers_choices() {
        let stream = |id: i64, external: bool, key: &str| plx_data::metadata::Stream { id, external, key: key.into(), ..Default::default() };
        let subs = [stream(1, false, ""), stream(2, false, ""), stream(1000, true, "/m/Film.de.srt")];
        let ids = sub_ids(&subs);
        assert_eq!(ids.iter().map(|s| s.choice.clone()).collect::<Vec<_>>(),
            [SubtitleChoice::Track(0), SubtitleChoice::Track(1), SubtitleChoice::File("Film.de.srt".into())]);
        let w = Watch {
            path: "/m/Film.mkv".into(), size: 7, audio_ids: vec![500, 501], subs: ids, seen: None,
            since: None, mounted: false, position_ms: 0, duration_ms: 0, ended: false,
        };
        let c = choices_now(&w, 501, 1000, -250);
        assert_eq!((c.audio, c.subtitle, c.subtitle_offset_ms, c.size), (Some(1), Some(SubtitleChoice::File("Film.de.srt".into())), -250, 7));
        assert_eq!(choices_now(&w, 500, 0, 0).subtitle, Some(SubtitleChoice::Off));
    }
}
