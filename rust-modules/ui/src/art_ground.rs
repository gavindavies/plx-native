//! `ArtGround` — a page standing on ONE title's own artwork (`PiTV Redesign.dc.html`, 1b and 1c).
//!
//! Two treatments of the same picture, one component:
//!
//! - **[`Treatment::Blurred`]** — the browsing ground (1b *Films*): the focused title's backdrop,
//!   decoded into a tiny box ([`BLUR_BOX`]) and stretched over the screen, so the texture filter
//!   does the blur for free — no blur pass, no extra framebuffer, a 48×27 upload. It is drawn at
//!   [`theme::ART_GROUND_BLUR_A`] under a veil that settles into the surface the shelves stand on.
//! - **[`Treatment::Sharp`]** — the hero ground (1c *Detail*): the backdrop full-bleed, anchored
//!   to its right edge (the subject of a key-art banner sits there, and the left third is where
//!   the title column reads), under the left wedge and the bottom veil that carry the text.
//!
//! **A change of title cross-dissolves.** [`ArtGround::key`] names what the page now stands on; the
//! old picture fades out under the new one on [`K_DISSOLVE`]. A title with no artwork keys `None`
//! and the ground dissolves to the bare surface. Motion is the spring's own report
//! (`gfx::spring`), so a settled ground asks for no frames.
//!
//! The component is generic library code: it takes a raw art source id and an opaque token, the
//! `ui::tex` seam, and never names the catalog that minted them.

use crate::consts::{SCR_H, SCR_W};
use crate::{theme, Painter, Rect, Spring};

/// The box a [`Treatment::Blurred`] picture is decoded into: so small that the bilinear stretch to
/// the screen IS the blur (one texel spans ~42 px, the design's `blur(70px)` at a glance).
pub const BLUR_BOX: (i32, i32) = (48, 27);
/// The box a [`Treatment::Sharp`] backdrop is decoded into: the canvas itself.
pub const SHARP_BOX: (i32, i32) = (SCR_W as i32, SCR_H as i32);
/// The dissolve's stiffness between two titles' grounds — the page wash's own pace, so the ground
/// and the focus pop above it settle together.
pub const K_DISSOLVE: f32 = 90.0;
/// How far past the screen the blurred picture reaches on every side, so its edge texels (which
/// clamp, and so read as flat bands) are off screen.
const BLUR_BLEED: f32 = 60.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Treatment {
    Blurred,
    Sharp,
}

/// One title's art: the raw source id and the token `ui::tex` resolves.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Art {
    sid: u16,
    token: String,
}

pub struct ArtGround {
    treatment: Treatment,
    cur: Option<Art>,
    prev: Option<Art>,
    /// 0 → 1 as `cur` replaces `prev`.
    mix: Spring,
}

impl ArtGround {
    pub fn new(treatment: Treatment) -> Self {
        Self { treatment, cur: None, prev: None, mix: Spring::at(1.0) }
    }

    /// The art the page now stands on. A first key is adopted whole (a page opens on its ground);
    /// a change dissolves from whatever is drawn now.
    pub fn key(&mut self, sid: u16, token: Option<&str>) {
        let next = token.filter(|t| !t.is_empty()).map(|t| Art { sid, token: t.to_owned() });
        if next == self.cur {
            return;
        }
        if self.cur.is_none() && self.prev.is_none() {
            self.cur = next;
            self.mix.jump(1.0);
            return;
        }
        // Mid-dissolve, the picture that is MORE visible is the one to leave from.
        if self.mix.pos >= 0.5 || self.prev.is_none() {
            self.prev = self.cur.take();
        }
        self.cur = next;
        self.mix.jump(0.0);
    }

    pub fn tick(&mut self, dt: f32) {
        if self.mix.pos < 1.0 || self.mix.vel != 0.0 {
            self.mix.step(1.0, K_DISSOLVE, dt);
            if (1.0 - self.mix.pos).abs() < 1e-3 && self.mix.vel.abs() < 1e-3 {
                self.mix.jump(1.0);
                self.prev = None;
            }
        }
    }

    /// How far the dissolve to the current art is (1 at rest). For tests and the page's own
    /// fades.
    pub fn settled(&self) -> bool {
        self.prev.is_none() && self.mix.pos >= 1.0
    }

    /// The decode box this ground asks `ui::tex` for.
    pub fn decode_box(&self) -> (i32, i32) {
        match self.treatment {
            Treatment::Blurred => BLUR_BOX,
            Treatment::Sharp => SHARP_BOX,
        }
    }

    /// Paint the ground over the whole canvas: the surface, the picture(s), then the veils.
    pub fn draw(&self, p: Painter) {
        p.rrect(Rect::FULL, 0.0, 0.0, theme::SURFACE_APP);
        let t = self.mix.pos.clamp(0.0, 1.0);
        if let Some(prev) = &self.prev {
            self.picture(p, prev, 1.0 - t);
        }
        if let Some(cur) = &self.cur {
            self.picture(p, cur, t);
        }
        match self.treatment {
            Treatment::Blurred => blurred_veil(p),
            Treatment::Sharp => sharp_veils(p),
        }
    }

    fn picture(&self, p: Painter, art: &Art, a: f32) {
        if a <= 0.0 {
            return;
        }
        let (bw, bh) = self.decode_box();
        let (tex, tw, th) = crate::tex::resolve_wh_on(art.sid, &art.token, bw, bh, false);
        if tex == 0 {
            return;
        }
        match self.treatment {
            Treatment::Blurred => {
                let r = Rect::new(-BLUR_BLEED, -BLUR_BLEED, SCR_W + 2.0 * BLUR_BLEED, SCR_H + 2.0 * BLUR_BLEED);
                p.tex_uv(tex, r.cover_uv(tw, th, plx_gfx::gfx::Crop::Centre), r, 0.0, theme::white(theme::ART_GROUND_BLUR_A * a));
            }
            Treatment::Sharp => {
                p.tex_uv(tex, right_cover_uv(Rect::FULL, tw, th), Rect::FULL, 0.0, theme::white(a));
            }
        }
    }
}

/// [`Rect::cover_uv`] with a wide source's kept window against its RIGHT edge (CSS
/// `object-position: right center`) rather than centred.
pub fn right_cover_uv(r: Rect, tw: f32, th: f32) -> [f32; 4] {
    let mut uv = r.cover_uv(tw, th, plx_gfx::gfx::Crop::Centre);
    if uv[2] < 1.0 {
        uv[0] = 1.0 - uv[2];
    }
    uv
}

/// The browsing veil: the surface at a fifth at the top, `.7` at 55%, and whole from 90% down —
/// so the shelves stand on the app's own surface and only the band above them is coloured.
fn blurred_veil(p: Painter) {
    let s = theme::surface;
    let y1 = SCR_H * 0.55;
    let y2 = SCR_H * 0.90;
    p.rect(Rect::new(0.0, 0.0, SCR_W, y1), 0.0, s(0.20), s(0.70), 0.0);
    p.rect(Rect::new(0.0, y1, SCR_W, y2 - y1), 0.0, s(0.70), s(1.0), 0.0);
    p.rrect(Rect::new(0.0, y2, SCR_W, SCR_H - y2), 0.0, 0.0, s(1.0));
}

/// The hero's two veils: a left wedge of scrim behind the title column (`.85` at the edge, `.55`
/// at 36%, clear by 60%), and a foot that settles into the surface the shelf below stands on.
fn sharp_veils(p: Painter) {
    let k = |a: f32| theme::scrim(a);
    let (x1, x2) = (SCR_W * 0.36, SCR_W * 0.60);
    p.grad4(Rect::new(0.0, 0.0, x1, SCR_H), [k(0.85), k(0.55), k(0.55), k(0.85)]);
    p.grad4(Rect::new(x1, 0.0, x2 - x1, SCR_H), [k(0.55), k(0.0), k(0.0), k(0.55)]);
    let s = theme::surface;
    let (y0, y1, y2) = (SCR_H * 0.55, SCR_H * 0.80, SCR_H * 0.90);
    p.rect(Rect::new(0.0, y0, SCR_W, y1 - y0), 0.0, s(0.0), s(0.90), 0.0);
    p.rect(Rect::new(0.0, y1, SCR_W, y2 - y1), 0.0, s(0.90), s(1.0), 0.0);
    p.rrect(Rect::new(0.0, y2, SCR_W, SCR_H - y2), 0.0, 0.0, s(1.0));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_key_is_adopted_whole_and_a_change_dissolves_until_it_settles() {
        let _g = plx_base::testlock::serial();
        let mut g = ArtGround::new(Treatment::Blurred);
        g.key(1, Some("L1"));
        assert!(g.settled(), "a page opens on its ground");
        g.key(1, Some("L2"));
        assert!(!g.settled());
        for _ in 0..240 {
            g.tick(1.0 / 60.0);
        }
        assert!(g.settled(), "the dissolve comes to rest and lets the old picture go");
        g.key(1, Some("L2"));
        assert!(g.settled(), "keying the same art again is not a change");
        g.key(1, None);
        assert!(!g.settled(), "losing the art dissolves to the bare surface");
    }

    #[test]
    fn a_wide_backdrop_keeps_its_right_edge() {
        let uv = right_cover_uv(Rect::new(0.0, 0.0, 1920.0, 1080.0), 4000.0, 1080.0);
        assert!((uv[0] + uv[2] - 1.0).abs() < 1e-6 && uv[0] > 0.0, "{uv:?}");
        let tall = right_cover_uv(Rect::new(0.0, 0.0, 1920.0, 1080.0), 1000.0, 1500.0);
        assert_eq!(tall[0], 0.0, "a picture cropped top and bottom is not moved sideways");
    }
}
