//! **The top tab bar's destinations** (`PiTV Redesign.dc.html` 1a/1b): Home, then the library
//! itself, one pill per place a viewer browses from, then Settings.
//!
//! - **Several library folders** (`PITV_MEDIA_DIRS=/srv/films:/srv/tv`): one pill per folder.
//! - **One folder holding plain folders** (`~/Videos` with `Films/` and `TV Shows/`): one pill per
//!   such folder — the folder is a container, its children are the libraries.
//! - **One folder of titles**: one pill, that folder.
//!
//! A pill's argument is the page it selects (`NavOp::SelectTab`), so the container's strip, the
//! app's labels and [`AppArg::chrome`](crate::registry::AppArg) all read this one list. It is
//! process state like the catalog it reads: the one-folder case changes when that folder's listing
//! lands, and the app republishes the pills then (their keys are positions in this list, which only
//! grows from one pill to its final count once, at boot).

use std::path::{Path, PathBuf};

use plx_data::local::{EntryKind, Listing};

use crate::registry::{AppArg, LocalArg};

/// The tab bar's pills, in order: `(label, the page it selects)`. Home is always first.
pub fn destinations() -> Vec<(String, AppArg)> {
    let mut out = vec![(plx_platform::i18n::msg::browse_chrome_home().to_owned(), AppArg::Home)];
    out.extend(libraries().into_iter().map(|(name, path)| (name, AppArg::Local(LocalArg { path }))));
    out.push((plx_platform::i18n::msg::settings_title().to_owned(), AppArg::Settings));
    out
}

/// The library pills: `(label, the folder page's path argument)`.
pub fn libraries() -> Vec<(String, String)> {
    let roots = plx_data::local::roots();
    let listing = match roots {
        [one] => Some(plx_data::local::listing(one)),
        _ => None,
    };
    choose(roots, listing.as_ref())
}

/// The pure half of [`libraries`]: `listing` is the single root's, when there is one root.
fn choose(roots: &[PathBuf], listing: Option<&Listing>) -> Vec<(String, String)> {
    let name = |p: &Path| p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
    if roots.len() != 1 {
        return roots.iter().map(|r| (name(r), r.display().to_string())).collect();
    }
    let root = &roots[0];
    if let Some(Listing::Ready(entries)) = listing {
        let plain: Vec<(String, String)> = entries.iter()
            .filter(|e| e.kind == EntryKind::Folder && !e.show)
            .map(|e| (e.display_title().to_owned(), e.path.display().to_string()))
            .collect();
        if !plain.is_empty() {
            return plain;
        }
    }
    // The single root's own page is the library's top level, whose argument is the empty path.
    vec![(name(root), String::new())]
}

/// Whether `arg` is a page the tab bar stands over: Home, Settings, or a library pill's folder.
pub fn is_tab_page(arg: &AppArg) -> bool {
    match arg {
        AppArg::Home | AppArg::Settings => true,
        AppArg::Local(a) => libraries().iter().any(|(_, path)| *path == a.path),
        _ => false,
    }
}

/// The pill (its strip element) that selects `arg`, when one does.
pub fn pill_elem(arg: &AppArg) -> Option<u32> {
    use plx_ui::screen::ScreenArg;
    destinations().iter().position(|(_, a)| a.same_instance(arg)).map(|i| plx_ui::dispatch::STRIP_BASE + i as u32)
}

/// **Where focus stands in the bar, for a tab page**: UP from the page reaches the SELECTED pill
/// (tvOS), and a page entered with focus on another pill — Home, returned to from the Films pill
/// that was pressed on it — moves it onto its own. The strip seats its remembered element
/// (`containers::tabs`), so the page keeps that memory pointed at its own pill: once on arrival,
/// and again whenever focus leaves the bar.
#[derive(Default)]
pub struct TabSeat {
    seeded: bool,
    arrived: bool,
}

impl TabSeat {
    /// Feed it every event the tab page `arg` (entry `entry`, instance `instance`) receives.
    pub fn on<H: crate::registry::ContentLike>(
        &mut self,
        arg: &AppArg,
        entry: plx_machine::machine::EntryId,
        instance: plx_machine::machine::InstanceId,
        ev: &plx_ui::screen::ScreenEvent<H>,
        cx: &plx_machine::machine::Cx<'_, H>,
        fx: &mut plx_machine::machine::Effects<'_, H>,
    ) {
        use plx_machine::machine::{Delivery, Enter, FocusKey, FocusTarget, Fx, MachineId};
        use plx_ui::dispatch::STRIP_BASE;
        use plx_ui::screen::ScreenEvent;
        let on_strip = |k: &FocusKey<u32>| k.entry == entry && k.elem >= STRIP_BASE;
        match ev {
            ScreenEvent::Enter(_) | ScreenEvent::Uncover => {
                self.seeded = false;
                self.arrived = true;
            }
            ScreenEvent::FocusMoved { from, to, .. } if from.is_some_and(|k| on_strip(&k)) && !on_strip(to) => {
                self.seeded = false;
            }
            ScreenEvent::Tick(_) => {
                let Some(own) = pill_elem(arg) else { return };
                if std::mem::take(&mut self.arrived) {
                    if cx.focus.current.is_some_and(|k| on_strip(&k) && k.elem != own) {
                        fx.push(Fx::Deliver(MachineId::Instance(instance), Delivery::Screen(ScreenEvent::Enter(
                            Enter::Fresh { focus: FocusTarget::Elem(FocusKey { entry, elem: own }) }))));
                    }
                }
                if !self.seeded && !cx.focus.current.is_some_and(|k| on_strip(&k)) {
                    self.seeded = true;
                    fx.remember(plx_ui::containers::tabs::STRIP, own);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plx_data::local::Entry;
    use std::sync::Arc;

    fn folder(path: &str, show: bool) -> Entry {
        let mut e = Entry::new(Path::new(path).file_name().unwrap().to_string_lossy().into_owned(), PathBuf::from(path), EntryKind::Folder, None, 0);
        e.show = show;
        e
    }

    #[test]
    fn several_roots_are_a_pill_each() {
        let roots = [PathBuf::from("/srv/Films"), PathBuf::from("/srv/TV Shows")];
        assert_eq!(choose(&roots, None), [("Films".into(), "/srv/Films".into()), ("TV Shows".into(), "/srv/TV Shows".into())]);
    }

    #[test]
    fn one_root_of_libraries_is_a_pill_per_library() {
        let roots = [PathBuf::from("/home/u/Videos")];
        let listing = Listing::Ready(Arc::new(vec![
            folder("/home/u/Videos/Films", false),
            folder("/home/u/Videos/Caminandes", true),
            folder("/home/u/Videos/TV Shows", false),
        ]));
        assert_eq!(choose(&roots, Some(&listing)), [
            ("Films".into(), "/home/u/Videos/Films".into()),
            ("TV Shows".into(), "/home/u/Videos/TV Shows".into()),
        ], "a show is a title, not a library");
    }

    #[test]
    fn one_root_of_titles_or_still_loading_is_one_pill() {
        let roots = [PathBuf::from("/srv/Films")];
        assert_eq!(choose(&roots, Some(&Listing::Loading)), [("Films".into(), String::new())]);
        let titles = Listing::Ready(Arc::new(vec![folder("/srv/Films/Caminandes", true)]));
        assert_eq!(choose(&roots, Some(&titles)), [("Films".into(), String::new())]);
    }
}
