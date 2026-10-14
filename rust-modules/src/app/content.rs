//! **The container requests an owned page raises, performed** — the library's, the player
//! overlays' and the item menu's. Each names a destination or an action the screen may not take
//! itself; this module turns it into a container op through `app::bridge` or into a call the loop
//! owns. A push carries the `ReturnState` the asking screen froze on ITS press frame rather than
//! one re-read at the commit.

use super::*;
use super::run::Frame;
use plx_screens::registry::ContentReq;
use plx_machine::machine::{InputOwner, MachineId, NavOp};

pub(crate) fn content_requests(app: &mut App, fr: &Frame) {
    // The player's overlays are surfaces on its own stack, so what they decide reaches the loop the
    // same way every other owned screen's decision does — as requests, drained here, after the
    // dispatcher and before the frame's own arms (`playback::player_requests`).
    let player_reqs = app.bridge.take_player_reqs();
    if !player_reqs.is_empty() {
        super::playback::player_requests(&mut app.player.session,
            &mut app.adapters.player,
            player_reqs,
            fr.now,
            &mut app.pages,
            &mut app.bridge,
            &mut app.ok_armed,
            &mut app.input.press,
            &mut app.repause_at,
        );
    }
    // …and the item context menu's committed row, for the same reason and at the same moment.
    for req in app.bridge.take_item_menu_reqs() {
        super::local_watch::apply_menu_action(&mut app.player.session, &mut app.adapters.player,
            req.act, &mut app.pages, &mut app.bridge);
    }
    for (source, request, ret) in app.bridge.take_content_reqs() {
        let MachineId::Instance(instance) = source else { continue };
        let Some(entry) = app.pages.nav.entry_of_instance(instance) else { continue };
        // A delayed effect cannot navigate for an instance that has already been covered.
        if app.pages.nav.input_owner() != Some(InputOwner::Entry(entry)) { continue; }
        match request {
            ContentReq::Back if bridge::nav_cancel(&mut app.pages) => {}
            ContentReq::Back if app.pages.nav.is_surface(entry) =>
                app.pages.request_with_return(source, NavOp::Dismiss(entry), ret),
            ContentReq::Back if app.pages.nav.pending_surface().is_some() =>
                app.pages.request_with_return(source, NavOp::Dismiss(app.pages.nav.pending_surface().unwrap()), ret),
            ContentReq::Back => bridge::nav_pop_with_return(&mut app.pages, ret),
            ContentReq::LocalOpen(arg) => {
                bridge::nav_push_with_return(&mut app.pages, AppArg::Local(arg), ret);
            }
            ContentReq::LocalDetail(arg) => {
                bridge::nav_push_with_return(&mut app.pages, AppArg::LocalDetail(arg), ret);
            }
            ContentReq::LocalPlay { path, from_start } => {
                super::local_watch::play(&mut app.player.session, &mut app.adapters.player, std::path::Path::new(&path),
                    from_start, Some(ret), &mut app.pages, &mut app.bridge);
            }
            ContentReq::ItemMenu => {
                // The page does not move: a surface is presented over the top page and never
                // replaces it.
                if let Some(arg) = app.bridge.content_menu_arg(&app.pages, entry, &ret) {
                    bridge::open_item_menu(&mut app.pages, arg);
                    app.ok_armed = false;
                }
            }
        }
    }
}
