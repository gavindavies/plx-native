//! Live executor for the player's preference picks: persistence lives here, never in a screen.
use plx_media::route::{Choice, How};
use plx_plex::plex::session::{DeckPressPref, DirectPlayModePref, NextEpisodeModePref, QualityPref,
    SkipIntervalPref, SubtitlePositionPref, SubtitleSizePref};
use std::sync::mpsc::Sender;

/// The one place a Settings pick meets the write door: each row's own `SETTINGS` policy
/// (`How::settings`) decides whether the pick is confirmed first or published at once.
pub(super) fn choose(choice: Choice, reply: Sender<bool>) {
    match choice {
        Choice::Quality(v) => plx_media::route::choose::<QualityPref>(v, How::settings::<QualityPref>(reply)),
        Choice::DirectPlay(v) => plx_media::route::choose::<DirectPlayModePref>(v, How::settings::<DirectPlayModePref>(reply)),
        Choice::NextEpisode(v) => plx_media::route::choose::<NextEpisodeModePref>(v, How::settings::<NextEpisodeModePref>(reply)),
        Choice::SkipInterval(v) => plx_media::route::choose::<SkipIntervalPref>(v, How::settings::<SkipIntervalPref>(reply)),
        Choice::DeckPress(v) => plx_media::route::choose::<DeckPressPref>(v, How::settings::<DeckPressPref>(reply)),
        Choice::SubtitleSize(v) => plx_media::route::choose::<SubtitleSizePref>(v, How::settings::<SubtitleSizePref>(reply)),
        Choice::SubtitlePosition(v) => plx_media::route::choose::<SubtitlePositionPref>(v, How::settings::<SubtitlePositionPref>(reply)),
    }
}
