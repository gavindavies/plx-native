//! Application-owned live backdrop schedule and chrome material state.
//! Every dynamic surface enters `backdrop` through the painter; the dispatcher supplies z bands.
//! The layer walk, geometric occlusion and region validity live in `backdrop`, with no route tests.

use crate::widgets::TabBand;
use super::backdrop::{Sources, Z};
use std::{rc::Rc, cell::RefCell};

/// The frame's glass schedule and the surfaces whose lifetime belongs to no screen.
pub struct GlassPlan {
    pub sources: Rc<RefCell<Sources>>,
    /// Persistent state for the shared top tab track: visible lifetime, adaptive density and this
    /// frame's material. The strip borrows it during paint; the Bridge never owns a second copy.
    tab: TabBand,
}

impl GlassPlan {
    pub fn new() -> Self {
        Self {
            sources: Rc::new(RefCell::new(Sources::default())),
            tab: TabBand::new(),
        }
    }

    pub fn walk(&self, ceiling: Z) -> super::backdrop::Scope {
        super::backdrop::enter(self.sources.clone(), ceiling)
    }

    pub fn step_tab_band(&mut self, dt: f32) {
        self.tab.step(dt);
    }

    pub fn tab_band_mut(&mut self) -> &mut TabBand {
        &mut self.tab
    }

    pub fn tab_face(&self) -> Option<plx_gfx::gfx::GlassFace> {
        self.tab.face()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn seed_tab_density_for_test(&mut self, value: f32) {
        self.tab.seed_density(value);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn tab_density_for_test(&self) -> f32 {
        self.tab.density()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_tab_face_for_test(&mut self, face: plx_gfx::gfx::GlassFace) {
        self.tab.set_face(face);
    }

}

impl Default for GlassPlan {
    fn default() -> Self {
        Self::new()
    }
}
