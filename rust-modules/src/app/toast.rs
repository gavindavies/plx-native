//! **The Linux build's toast: the television's system notification, drawn by the app itself.**
//!
//! On a television a toast is the platform's own UI (`plx_platform::tv::toast`, delivered over
//! LS2 by the webOS port). An ordinary Linux machine has no such service that would show over a
//! full-screen app, so the Linux build's port delivers here instead ([`deliver`]) and the frame
//! loop draws the message over every route, the player's included ([`draw`]). Everything that
//! raises a toast — the clock notice, a local file the player refuses — goes through the same
//! door it uses on a set.
//!
//! Like the lab read-out it takes NO KEYS: it is not a route and has no dismiss. The newest
//! message replaces the one on screen and stays for [`SHOW_MS`].
use plx_platform::tv::toast::{Identity, Outcome, Sent};
use plx_ui::consts::SAFE;
use plx_ui::label::Label;
use plx_ui::{theme, Painter, Rect};
use std::ffi::CString;
use std::sync::Mutex;

/// How long a toast stays up.
const SHOW_MS: u32 = 6_000;
const W: f32 = 1200.0;
const H: f32 = 88.0;
const PAD: f32 = 32.0;

/// The message on screen and the frame time it comes down — `None` until the first frame that
/// sees it starts its window ([`update`]), since it arrives on a worker that has no frame clock.
static SHOWN: Mutex<Option<(String, Option<u32>)>> = Mutex::new(None);

/// The port's `deliver_toast`. Called on a worker (`tv::toast::toast` blocks on a television);
/// here it only records the message and wakes the frame loop to paint it.
pub(crate) fn deliver(message: &str, identity: Identity) -> Sent {
    plx_base::eventlog::log(&format!("toast: shown in-app ({identity:?}): {message}"));
    *SHOWN.lock().unwrap_or_else(|e| e.into_inner()) = Some((message.to_owned(), None));
    // A clock-driven overlay, not a spring: without the invalidate it would appear only on the
    // next keypress (see `lab::upload::set_phase`).
    plx_machine::idle::invalidate();
    Sent { reply: None, outcome: Outcome::Accepted }
}

/// Raise `message` from the frame thread: the platform call blocks, so it runs on a worker,
/// exactly as the clock notice sends its own.
pub(crate) fn raise(message: String) {
    plx_base::task::spawn_small("toast", move || {
        let _ = plx_platform::tv::toast::toast(&message);
    });
}

/// Start a new toast's window and retire an expired one. Once per frame from the update block,
/// with the frame's clock; the invalidate gets the frame that paints its absence.
pub(crate) fn update(now: u32) {
    let mut shown = SHOWN.lock().unwrap_or_else(|e| e.into_inner());
    let Some((_, until)) = shown.as_mut() else { return };
    match *until {
        None => *until = Some(now.wrapping_add(SHOW_MS)),
        // Wrapping, like every other frame-clock comparison: past the deadline is a small
        // non-negative difference.
        Some(t) if (now.wrapping_sub(t) as i32) >= 0 => {
            *shown = None;
            plx_machine::idle::invalidate();
        }
        Some(_) => {}
    }
}

/// Bottom centre, inside the overscan frame, where a television's own toasts are not.
fn frame() -> Rect {
    Rect::new(SAFE.x + (SAFE.w - W) / 2.0, SAFE.y + SAFE.h - H, W, H)
}

/// Draw the toast, over everything, on every route.
pub(crate) fn draw() {
    let Some(message) = SHOWN.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|(m, _)| m.clone()) else {
        return;
    };
    let r = frame();
    let p = Painter::root();
    // Its own opaque ground: on the player route the UI plane is cleared transparent, and the
    // text has to survive a bright frame of video behind it.
    p.rect(r, 20.0, theme::PANEL_TOP, theme::PANEL_BOT, 0.0);
    if let Ok(cs) = CString::new(message) {
        Label::new(cs.as_ptr(), theme::size::BODY, theme::TEXT_PRIMARY)
            .bold()
            .draw(p, Rect::new(r.x + PAD, r.y + (H - 34.0) / 2.0, r.w - 2.0 * PAD, 34.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn showing() -> bool {
        SHOWN.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    #[test]
    fn a_toast_stays_its_window_from_the_first_frame_that_sees_it() {
        let _ = deliver("first", Identity::AsApp);
        // The window starts on the frame that first sees it, even across the clock's wrap.
        let start = u32::MAX - 1_000;
        update(start);
        update(start.wrapping_add(SHOW_MS - 1));
        assert!(showing(), "still inside its window");
        update(start.wrapping_add(SHOW_MS));
        assert!(!showing(), "retired when the window ends");
        // A newer message replaces the one up and gets a window of its own.
        let _ = deliver("second", Identity::AsApp);
        update(10);
        let _ = deliver("third", Identity::AsApp);
        update(10 + SHOW_MS);
        assert!(showing(), "the replacement's window starts when it is first seen");
    }

    #[test]
    fn the_toast_stays_inside_the_safe_area() {
        let r = frame();
        assert!(r.x >= SAFE.x && r.x + r.w <= SAFE.x + SAFE.w);
        assert!(r.y >= SAFE.y && r.y + r.h <= SAFE.y + SAFE.h);
    }
}
