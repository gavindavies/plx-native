//! **The top tab bar, published and painted** (`PiTV Redesign.dc.html` 1a/1b).
//!
//! The library owns the bar's mechanics — the container's pills and strip group
//! (`ui::containers::tabs`), the focus engine's walk into and along it, and the glass track with its
//! travelling capsules (`ui::widgets::StripRender`). What the application supplies is the
//! vocabulary and the geometry, every frame, before the dispatcher runs:
//!
//! - the pills (`plx_screens::tabs::destinations`: Home, then the library's folders), as the
//!   arguments `NavOp::SelectTab` selects, keyed `STRIP_BASE + position`;
//! - the strip's members, from the same measured widths the paint lays out — only while the page on
//!   top wears the bar (`Chrome::TabBar`), so focus can never climb into a bar nobody can see;
//! - which pill is selected (the one whose page is on top) and which is focused.
//!
//! The press is the page's (`ScreenEvent::Activate` of a strip element → `ContentReq::SelectTab`,
//! performed by `content::content_requests`), and the paint is [`TabChrome::draw`], which the
//! dispatcher calls through `Rig::draw_chrome` after the page and before any surface.

use plx_machine::machine::{InputOwner, Measure};
use plx_screens::registry::AppArg;
use plx_ui::dispatch::{Dispatcher, STRIP_BASE};
use plx_ui::screen::ScreenArg;
use plx_ui::widgets::{tab_members, tab_widths, StripRender, TabBand, TabLabels, TopFocus};

use super::bridge::AppHost;

pub(crate) struct TabChrome {
    strip: StripRender,
    band: TabBand,
    labels: Vec<String>,
    /// Moves when the labels do: the strip's glyph memo is keyed by it.
    generation: u32,
    selected: usize,
}

impl TabChrome {
    pub(crate) fn new() -> Self {
        Self { strip: StripRender::new(), band: TabBand::new(), labels: Vec::new(), generation: 0, selected: 0 }
    }

    /// Publish this frame's pills and strip into the container, and step the bar's motion.
    pub(crate) fn publish(&mut self, d: &mut Dispatcher<AppHost>, measure: &dyn Measure, dt: f32) {
        let dests = plx_screens::tabs::destinations();
        let labels: Vec<String> = dests.iter().map(|(label, _)| label.clone()).collect();
        if labels != self.labels {
            self.labels = labels;
            self.generation = self.generation.wrapping_add(1);
        }
        d.nav.tabs.pills = dests.into_iter().map(|(_, arg)| arg).collect();
        d.nav.tabs.strip.clear();
        let Some(top) = d.nav.top_page().filter(|e| e.arg.chrome() == plx_machine::machine::Chrome::TabBar) else {
            return;
        };
        let (top_id, top_arg) = (top.id, top.arg.clone());
        self.selected = selected_of(&d.nav.tabs.pills, &top_arg);
        d.nav.tabs.selected = self.selected;
        let focus = d.input.engine.read(InputOwner::Entry(top_id)).current
            .filter(|k| k.entry == top_id && k.elem >= STRIP_BASE)
            .map_or(TopFocus::Away, |k| TopFocus::Pill((k.elem - STRIP_BASE) as usize));
        let keys: Vec<u32> = (0..self.labels.len() as u32).map(|i| STRIP_BASE + i).collect();
        let widths = tab_widths(&self.labels, measure);
        tab_members(&widths, &keys, self.selected as i32, focus, self.strip.scroll_pos(), &mut d.nav.tabs.strip);
        let data = TabLabels { generation: self.generation, labels: &self.labels };
        self.strip.update(data, self.selected as i32, focus, dt);
        self.band.step(dt);
    }

    /// Paint the bar — `Rig::draw_chrome`, only ever for a page that wears it.
    pub(crate) fn draw(&mut self, chrome_alpha: f32) {
        let p = plx_ui::Painter::root().alpha(chrome_alpha);
        let data = TabLabels { generation: self.generation, labels: &self.labels };
        self.strip.draw(data, p, &mut self.band);
    }
}

/// The pill whose page is `top` (Home when none is).
fn selected_of(pills: &[AppArg], top: &AppArg) -> usize {
    pills.iter().position(|a| a.same_instance(top)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plx_screens::registry::LocalArg;

    #[test]
    fn the_selected_pill_is_the_one_whose_page_is_on_top() {
        let local = |p: &str| AppArg::Local(LocalArg { path: p.into() });
        let pills = [AppArg::Home, local("/srv/Films"), local("/srv/TV Shows")];
        assert_eq!(selected_of(&pills, &local("/srv/TV Shows")), 2);
        assert_eq!(selected_of(&pills, &AppArg::Home), 0);
        assert_eq!(selected_of(&pills, &local("/srv/Films/Silent")), 0, "a page under no pill falls back to Home");
    }
}
