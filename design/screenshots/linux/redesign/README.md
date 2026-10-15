# Redesign captures

The tvOS-style redesign (`PiTV Redesign.dc.html`, stage 1: Films shelves, the title page, a season shelf; then the focused poster's tilt; then Home and the tab bar; then the player HUD — top menu, scrub preview, Info card, track and chapter panels — and the title page's track choice; then the Settings tab, a picker, the live subtitle preview, and the playbar's buffered band; then Settings › Library with a folder, a connected SMB share and one that would not connect, the share's own tab, and the library after a rescan), captured on the demo library with `PITV_MEDIA_DIRS` set to its `Films` and `TV Shows` folders. The artwork is credited in [`../CREDITS.md`](../CREDITS.md).

They are rendered at **3840x2160** — a real 4K render, not an upscale: `PITV_RENDER_SCALE=2` draws the 1920x1080 canvas into a 2x offscreen framebuffer with glyphs, icons and artwork rasterised at 2x (`plx_base::surface::render_scale`). The video under the player shots is the demo clip's own 720p picture, scaled.

`18-player-buffered.jpg` plays a generated 20 Mbps test pattern (not in the demo library) with Read ahead set to All files and a 64 MB buffer, so the band ends partway along the rail; the demo films are small enough to buffer whole.

`19`–`23` read a test `library.conf` (`PITV_CONFIG`): a folder of the demo library, the demo library served by a local Samba server and mounted through gio, and an address no server answers at. `23` is after a section was added to the file and Rescan library chosen.
