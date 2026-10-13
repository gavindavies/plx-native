//! The transport-pause contract the loop spells (`paused`, `viewer_paused`, `set_transport_paused`,
//! ...). It describes the PLAYER's transport, so it lives in [`plx_media::player::lifecycle`]
//! (module-layer step L12: `player` may not name `app`) and is re-exported here, which keeps every
//! `app` caller spelling the names it always did.

pub(crate) use plx_media::player::lifecycle::{
    paused, resume_if_paused, set_paused, set_transport_paused, transport_target, viewer_paused,
};
