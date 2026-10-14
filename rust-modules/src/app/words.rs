//! **The heartbeat's `route=` and ` overlay=` alphabet**: the top page's word and the topmost
//! surface's, as the once-a-second heartbeat line prints them.

use super::bridge;
use plx_screens::registry::AppArg;

/// The heartbeat's `route=` WORD for a page.
pub(crate) fn route_word(route: &AppArg) -> &'static str {
    match route {
        AppArg::Player => "player",
        AppArg::Local(_) => plx_screens::registry::word::LOCAL,
        AppArg::LocalDetail(_) => plx_screens::registry::word::LOCAL_DETAIL,
        AppArg::Home => plx_screens::registry::word::HOME,
        // Not pages: a surface's word is ` overlay=`, and none of these reaches the page stack's
        // top. Written out rather than swept into a `_` so a new PAGE variant is a compile error.
        AppArg::ItemMenu(_) | AppArg::PlayerOverlay(_) => "",
    }
}

/// The heartbeat's ` overlay=` WORD, or `None` when nothing is over the page: the topmost
/// surface's own `Screen::name`, asked FIRST. [`NO_OVERLAY`] is the one word no screen owns: the
/// player with nothing over it prints ` overlay=none`, a statement about the PAGE that the
/// container cannot make.
pub(crate) fn overlay_word(pages: &plx_ui::dispatch::Dispatcher<bridge::AppHost>, route: &AppArg) -> Option<&'static str> {
    bridge::overlay_word(pages).or(matches!(route, AppArg::Player).then_some(NO_OVERLAY))
}

/// The bare player, whose ` overlay=` word belongs to no screen — see [`overlay_word`].
pub(crate) const NO_OVERLAY: &str = "none";

/// The heartbeat's ` overlay=<word>` suffix, prefix and all, empty when there is none.
pub(crate) fn overlay_suffix(pages: &plx_ui::dispatch::Dispatcher<bridge::AppHost>, route: &AppArg) -> String {
    overlay_word(pages, route).map_or(String::new(), |w| format!(" overlay={w}"))
}

#[cfg(test)]
mod heartbeat_word_tests {
    use super::*;

    /// Every page prints a word and every surface prints one too — derived from the real mounter
    /// (`bridge::every_surface_word`), so the alphabet is what the screens say it is.
    #[test]
    fn every_page_and_surface_names_a_word() {
        let _serial = plx_base::testlock::serial();
        for page in [AppArg::Player, AppArg::Local(plx_screens::registry::LocalArg { path: String::new() })] {
            assert!(!route_word(&page).is_empty());
        }
        let words = bridge::every_surface_word();
        assert!(words.iter().all(|w| !w.is_empty()));
        assert!(words.contains(&plx_screens::registry::word::ITEM_MENU));
    }
}
