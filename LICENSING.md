# Licensing

PiTV is a modified version of PlxNative (see `NOTICE.md`). PlxNative: Copyright (c) 2026 Gleb
Linnik and contributors; modifications: Copyright (c) 2026 Gavin Davies.

The project's own code is licensed under the GNU General Public License, version 3 or (at your
option) any later version: **GPL-3.0-or-later**. The complete license is in `LICENSE`. There is no
warranty, to the extent permitted by applicable law.

You may use, modify and redistribute this software, including selling copies, under those terms.
Third-party components retain their own licenses and copyrights; see `THIRD-PARTY-NOTICES.md`
and `licenses/`. Earlier MIT permissions remain available for the PlxNative versions and
contributions originally offered under them; their original grant is preserved in
`licenses/PlxNative-historical-MIT.txt`. The GPL election does not revoke old grants. Trademark
guidance is separate in `TRADEMARKS.md` and does not narrow the copyright license.

The system libraries the application loads at run time (GStreamer, SDL2, GLib, the C library and
the GL driver) are not distributed with it; they are System Libraries under GPL section 1.

Before conveying a binary, make its matching complete source (including `ci/build-ffmpeg.sh`,
`ci/build-libass.py` and the pinned upstream archives they fetch) available alongside it, as the
GPL and LGPL require.
