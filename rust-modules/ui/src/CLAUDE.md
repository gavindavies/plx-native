# ui/ — the shared UI system (read before touching any screen)

This directory is the workspace crate `plx_ui` (`rust-modules/ui/`). The application names its
items as `plx_ui::theme`, `plx_ui::widgets`, ... It holds no application type: screens live in
`plx_screens` (`../../screens/src/CLAUDE.md`), shared application widgets in `plx_appkit`.

This directory is a **design system**, not a pile of per-screen draw code. Home, the library, the
title page and the player HUD are all *compositions of the same tokens + components*. When you add
or change UI, **reach for the shared pieces and improve them in place** — do not paste a new
`draw_*` function with hard-coded colours and hand-tuned text offsets. If a shared piece is missing,
build it here as a reusable component. Design material lives in `design/` (start at
`design/README.md`).

## The four rules

1. **Never write a raw color literal.** Every color comes from `theme.rs` as a named token
   (`theme::TEXT_PRIMARY`, `theme::CONTROL_IDLE_FILL`, `theme::scrim(a)`, …). Need a shade that
   doesn't exist yet? **Add a token to `theme.rs`** (with a doc line saying what it's for) and use
   that — don't inline `[0.9, 0.9, 0.9, 1.0]`. If your shade is within a hair of an existing token,
   use the existing one; the point is one value per role, not a value per call site.
   `theme.rs` is **two layers**: a token is a **role** (what the colour is *for*) that resolves to a
   **primitive** (a stop on the palette, private to that module). Reach for an existing stop first;
   two roles landing on one stop stay two roles. Overlays are `with_a(WHITE, a)` on the measured
   alpha ramp — a new overlay is a weight, never a new hue.

2. **Never write a raw text size — every text size is a `theme::size` token.** The rungs are
   `size::HERO` 72 / `DISPLAY` 48 / `TITLE` 40 / `HEADLINE` 32 / `BODY` 28 / `LABEL` 26 /
   `CAPTION` 24 / `MICRO` 22 / `DIAGNOSTIC` 20. `CAPTION` is the couch legibility floor for
   ordinary content; `MICRO` is only for one-line de-emphasized labels. Pass a rung to
   `Painter::text`/`Label`/`TextView`/`text::elide`; if no rung fits a genuinely new role, add a
   documented rung to `pub mod size` in `gfx/src/gfx/tokens.rs` (re-exported as `theme::size`).
   Two named carve-outs exist (the player-HUD title `HUD_TITLE_SZ` and the subtitle caption) —
   don't add a third.

3. **Never hand-place text with a magic y.** `y - sz*0.58` guesses are banned — they mis-center the
   moment a string has a descender. Text is positioned by its **cap band**: use `ui::label::Label`
   (single run) or `ui::text_view::TextView` (multi-line, pixel-wrapped), or derive the y from
   `text::text_vcenter_y` / `text::text_cap_band`. See `label.rs`'s module docs.

4. **Improve a component before forking one.** If a shared widget almost does what you need, add a
   builder method / style variant to it (e.g. `Button::style(ControlStyle)`), don't copy it. A new
   bespoke widget lands *here*, as a reusable component, so the next screen gets it for free.

## Components to reach for

- **Card strips and grids:** `cards::{Shelf, Grid, Stack}` over a `CardSource` the screen
  implements. The contract is the `//!` of `cards/mod.rs`; `cards/tests.rs` and
  `cards/conformance.rs` grade it.
- **Menus are popovers, never full-screen sheets:** a `popover.rs` container over the live screen
  with a `table.rs` `TableView` inside (see `appkit/src/track_menu.rs`). A menu never opens with its
  focus on a destructive action (`Row::destructive(true)` + `TableView::open_sections`).
- **Forms and settings pages:** `form.rs` (`Form`/`FormTable`: declared rows with stable keys),
  `table.rs` (`TableView`), `table_screen.rs` (`TablePart`/`TableScreen`/`Header`) and
  `text_buffer.rs` for text entry.
- **The screen model:** `screen.rs` (`Screen`, `Composed`/`Part`, `DrawFrame`), `focus.rs` (the
  focus engine), `dispatch.rs` (the one frame algorithm), `containers/` (tabs, nav stack, modals,
  transitions).

## Product idiom (settled owner verdicts)

1. **Clickable text marks are ALL CAPS** (`text_view::more_mark()`), overriding sentence case.
2. **The player HUD gets a state glyph, not a transport row.** No `<<` `||` `>>` buttons; transport
   is driven by remote keys. The HUD's top menu holds panels (Info, Chapters, Subtitles, Audio).
3. **Failure read-outs are never red** — a failed verdict is inked bold at `size::TITLE` in
   `theme::TEXT_SECONDARY`.
4. **Read the design-system contract before inventing a control state** (focus, hover, selected).

## Where things live

`lib.rs` is the map: every module is declared there with its purpose. The `//!` doc at the top of
each file is the authority on that module — read it before changing the file, and give a new module
both a `//!` doc and a `lib.rs` line.

## Gotchas

- **`Label`/`Button` hold a non-owning `*const c_char`.** Keep the `CString` alive for the whole
  draw frame. (`TextView` borrows a `&str` and is memory-safe.)
- **The focus ring/glow is shader-baked** (`gfx.rs`): callers drive it only through a `focus: f32`
  scalar and the geometry consts. Don't re-add it as a literal colour.
- **Clipping: prefer culling.** `Painter::clip`/`clip_clear` is global GL state — pair them in the
  same frame. A screen drawing through a `DrawFrame` uses `f.clip(p, rect)` (RAII). Long scrolling
  content culls off-frame children by index instead.
- **Process-global state in tests:** anything that touches a shared store or reports to
  `plx_machine::idle` takes `plx_base::testlock::serial()` in its tests.
- **Anything that animates owes a test** that it reports motion while running and goes quiet at
  rest (`machine/src/idle.rs`'s tests are the pattern) — an animator that never settles keeps the
  frame loop awake.

## When you're done

`cargo +nightly test --workspace --lib` (see `AGENTS.md`), then look at the pixels: run the app
headless and take a screenshot through the remote FIFO (`AGENTS.md`, "Headless run").
