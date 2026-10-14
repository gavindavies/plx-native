# Redesign captures

The tvOS-style redesign (`PiTV Redesign.dc.html`, stage 1: Films shelves, the title page, a season shelf; then the focused poster's tilt; then Home and the tab bar; then the player HUD — top menu, scrub preview, Info card, track and chapter panels — and the title page's track choice), captured on the demo library with `PITV_MEDIA_DIRS` set to its `Films` and `TV Shows` folders. The artwork is credited in [`../CREDITS.md`](../CREDITS.md).

They are rendered at **3840x2160** — a real 4K render, not an upscale: `PITV_RENDER_SCALE=2` draws the 1920x1080 canvas into a 2x offscreen framebuffer with glyphs, icons and artwork rasterised at 2x (`plx_base::surface::render_scale`). The video under the player shots is the demo clip's own 720p picture, scaled.
