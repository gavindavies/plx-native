# Third-party notices

This file accompanies **PiTV**, a media player for local and SMB-mounted video files on Linux
(a modified version of PlxNative; see `NOTICE.md`). PiTV's own code is distributed under
GPL-3.0-or-later (see `LICENSE` and `LICENSING.md`). It is staged beside the binary by `make` and
travels with the application directory.

The file is organised by **relationship**: what is bundled beside the binary, what is compiled into
it, and what is a system library the binary loads at run time but that is not distributed with it.
Full licence texts are in `licenses/` (and `OFL.txt` for the fonts), not repeated inline.

## 1. Bundled beside the binary

### 1.1 FFmpeg

| Shipped file | Upstream | Licence |
|---|---|---|
| `libavutil-plx.so.61`, `libavcodec-plx.so.63`, `libavformat-plx.so.63` | FFmpeg **9.0**, unmodified | LGPL-2.1-or-later (`licenses/LGPL-2.1.txt`) |

**Complete corresponding source.** `ffmpeg-9.0.tar.xz`, sha256
`7f607a00dd0d28a729d5a4811205812eef01cf6ef6155025febb6f36a9062d52`, from
<https://ffmpeg.org/releases/>; no patches are applied. The complete configure invocation,
including every enabled component, is `ci/build-ffmpeg.sh`.

**Configuration.** `--disable-everything` plus an explicit component list, **without**
`--enable-gpl`, `--enable-version3` or `--enable-nonfree`. Only demuxers, parsers, bitstream
filters and subtitle decoders are enabled — video and audio are decoded by GStreamer. The one
external library enabled is the system's **zlib** (not redistributed).

**You may modify and replace them.** They are ordinary shared libraries, loaded at run time by
`dlopen` from the application's directory; no FFmpeg code is linked into the `pitv` executable.
Build your own from the source above and put it in the app directory under the same file name. The
application refuses to demux if the library reports different major versions.

### 1.2 Native ASS rendering

`libass-plx-host.so.0` contains the PiTV facade (`src/ass.c`, `include/ass.h`, GPL-3.0-or-later)
and this pinned, statically linked stack:

| Component | Version | Licence |
|---|---|---|
| libass | 0.17.5 | ISC (`licenses/libass-ISC.txt`) |
| FreeType | 2.14.3 | Elected FreeType License; contributed MIT and Zlib code (`licenses/FreeType.txt`, `licenses/Zlib.txt`) |
| FriBidi | 1.0.17 | LGPL-2.1-or-later (`licenses/LGPL-2.1.txt`) |
| HarfBuzz | 14.5.0 | Old MIT (`licenses/HarfBuzz-Old-MIT.txt`) |

Portions of this software are copyright © 2026 The FreeType Project (https://freetype.org). All
rights reserved. Its built-in gzip reader contains zlib 1.3.1, Copyright (C) 1995-2024 Jean-loup
Gailly and Mark Adler. FriBidi is Copyright (C) 1999, 2000, 2017-2019 Dov Grobgeld; 2001, 2002,
2004, 2005 Behdad Esfahbod; 2004 Sharif FarsiWeb, Inc.; and other contributors; the complete
per-file notices accompany its source. libass and HarfBuzz copyright notices are retained in their
licence files above.

The exact upstream archives, checksums and licences are recorded in `ci/libass-dependencies.json`;
the complete build recipe is `ci/build-libass.sh` / `ci/build-libass.py`. Upstream sources are
unmodified. You may modify and rebuild the complete library, including FriBidi, and replace
`libass-plx-host.so.0` in the app directory.

### 1.3 Fonts

**Inter** — `appfont.ttf`, `appfont-bold.ttf`. Copyright 2016 The Inter Project Authors
(https://github.com/rsms/inter), version 4.001. Licence: **SIL Open Font License 1.1**
(`OFL.txt`). *Modified*: static instances cut from the Inter variable font at weight 400 / 700 and
optical size 18, with tabular figures frozen and a legacy `kern` table synthesised. Inter declares
no Reserved Font Name, so the family name is retained. "Inter UI and Inter is a trademark of rsms."

## 2. Compiled into `pitv`

### 2.1 Vendored C source

**nanosvg** and **nanosvgrast** — Copyright (c) 2013-14 Mikko Mononen <memon@inside.org>.
Licence: **Zlib** (`licenses/Zlib.txt`). Byte-identical to upstream
(github.com/memononen/nanosvg). The upstream headers credit: the SVG parser is based on the
**Anti-Grain Geometry 2.4** SVG example (Copyright (C) 2002-2004 Maxim Shemanarev); arc calculation
code is from **canvg**; bounding-box calculation follows blog.hackers-cafe.net; the polygon
rasterizer is based on the **stb_truetype** rasterizer by Sean Barrett.

### 2.2 Icon artwork

SVG icons from `design/icons/` are embedded in the binary. Most are original work. These derive
from third-party sets, all visually modified (stroke width and/or colour):

- **Google Material Design Icons** — `user.svg` (the `person` icon). Copyright Google LLC.
  **Apache License 2.0** (`licenses/Apache-2.0.txt`). *Modified*: transparent bounding-box path
  removed, explicit white fill added.
- **Feather Icons** — `backspace.svg` (`delete`). Copyright (c) 2013-2023 Cole Bemis. **MIT**
  (`licenses/MIT.txt`). *Modified*: stroke width 2 → 2.2, white stroke. The chevron icons use the
  same vertex coordinates as Feather's chevrons and are credited under the same notice.
- **Heroicons** — `check.svg`. Copyright (c) Tailwind Labs, Inc. **MIT** (`licenses/MIT.txt`).
  *Modified*: stroke width 2 → 3, white stroke.

### 2.3 Rust code

The application is Rust, statically linking the Rust standard library (MIT OR Apache-2.0,
Copyright The Rust Project Developers; rustc nightly) and the crates below. Where a crate offers a
choice of licences, MIT is elected where offered, else Apache-2.0; election does not alter your
rights under the other arms. Each crate's copyright notice is in its own source distribution
(crates.io); the licence texts are `licenses/MIT.txt`, `licenses/Apache-2.0.txt`,
`licenses/Zlib.txt` and, for ICU4X, `licenses/ICU4X.txt`.

| Crate | Version | Declared licence |
|---|---|---|
| adler2 | 2.0.1 | 0BSD OR MIT OR Apache-2.0 |
| atomic_refcell | 0.1.14 | Apache-2.0 OR MIT |
| bitflags | 2.13.0 | MIT OR Apache-2.0 |
| bytemuck | 1.25.0 | Zlib OR Apache-2.0 OR MIT |
| byteorder-lite | 0.1.0 | Unlicense OR MIT |
| calendrical_calculations | 0.2.4 | Apache-2.0 |
| cfg-if | 1.0.4 | MIT OR Apache-2.0 |
| core_maths | 0.1.1 | MIT |
| crc32fast | 1.5.0 | MIT OR Apache-2.0 |
| displaydoc | 0.2.7 | MIT OR Apache-2.0 |
| either | 1.18.0 | MIT OR Apache-2.0 |
| fdeflate | 0.3.7 | MIT OR Apache-2.0 |
| fixed_decimal | 0.7.2 | Unicode-3.0 |
| flate2 | 1.1.9 | MIT OR Apache-2.0 |
| futures-channel | 0.3.34 | MIT OR Apache-2.0 |
| futures-core | 0.3.34 | MIT OR Apache-2.0 |
| futures-executor | 0.3.34 | MIT OR Apache-2.0 |
| futures-macro | 0.3.34 | MIT OR Apache-2.0 |
| futures-sink | 0.3.34 | MIT OR Apache-2.0 |
| futures-task | 0.3.34 | MIT OR Apache-2.0 |
| futures-util | 0.3.34 | MIT OR Apache-2.0 |
| gio-sys | 0.22.9 | MIT |
| glib | 0.22.10 | MIT |
| glib-macros | 0.22.9 | MIT |
| glib-sys | 0.22.9 | MIT |
| gobject-sys | 0.22.9 | MIT |
| gstreamer | 0.25.4 | MIT OR Apache-2.0 |
| gstreamer-app | 0.25.2 | MIT OR Apache-2.0 |
| gstreamer-app-sys | 0.25.4 | MIT |
| gstreamer-base | 0.25.4 | MIT OR Apache-2.0 |
| gstreamer-base-sys | 0.25.4 | MIT |
| gstreamer-sys | 0.25.4 | MIT |
| heck | 0.5.0 | MIT OR Apache-2.0 |
| icu_calendar | 2.1.1 | Unicode-3.0 |
| icu_calendar_data | 2.1.1 | Unicode-3.0 |
| icu_collections | 2.1.1 | Unicode-3.0 |
| icu_datetime | 2.1.1 | Unicode-3.0 |
| icu_datetime_data | 2.1.2 | Unicode-3.0 |
| icu_decimal | 2.1.1 | Unicode-3.0 |
| icu_decimal_data | 2.1.1 | Unicode-3.0 |
| icu_locale | 2.1.1 | Unicode-3.0 |
| icu_locale_core | 2.3.0 | Unicode-3.0 |
| icu_locale_data | 2.1.2 | Unicode-3.0 |
| icu_pattern | 0.4.2 | Unicode-3.0 |
| icu_plurals | 2.1.1 | Unicode-3.0 |
| icu_plurals_data | 2.1.1 | Unicode-3.0 |
| icu_provider | 2.3.1 | Unicode-3.0 |
| icu_time | 2.1.1 | Unicode-3.0 |
| icu_time_data | 2.1.1 | Unicode-3.0 |
| image | 0.25.10 | MIT OR Apache-2.0 |
| itertools | 0.15.0 | MIT OR Apache-2.0 |
| itoa | 1.0.18 | MIT OR Apache-2.0 |
| ixdtf | 0.6.6 | Unicode-3.0 |
| kstring | 2.0.5 | MIT OR Apache-2.0 |
| libc | 0.2.186 | MIT OR Apache-2.0 |
| libm | 0.2.16 | MIT |
| litemap | 0.8.3 | Unicode-3.0 |
| memchr | 2.8.2 | Unlicense OR MIT |
| miniz_oxide | 0.8.9 | MIT OR Zlib OR Apache-2.0 |
| moxcms | 0.8.1 | BSD-3-Clause OR Apache-2.0 |
| muldiv | 1.0.1 | MIT |
| num-integer | 0.1.47 | MIT OR Apache-2.0 |
| num-rational | 0.4.2 | MIT OR Apache-2.0 |
| num-traits | 0.2.19 | MIT OR Apache-2.0 |
| option-operations | 0.6.1 | MIT/Apache-2.0 |
| pastey | 0.2.3 | MIT OR Apache-2.0 |
| pin-project-lite | 0.2.17 | Apache-2.0 OR MIT |
| png | 0.18.1 | MIT OR Apache-2.0 |
| potential_utf | 0.1.6 | Unicode-3.0 |
| proc-macro2 | 1.0.106 | MIT OR Apache-2.0 |
| pxfm | 0.1.29 | BSD-3-Clause OR Apache-2.0 |
| quote | 1.0.46 | MIT OR Apache-2.0 |
| serde | 1.0.228 | MIT OR Apache-2.0 |
| serde_core | 1.0.228 | MIT OR Apache-2.0 |
| serde_derive | 1.0.228 | MIT OR Apache-2.0 |
| serde_json | 1.0.150 | MIT OR Apache-2.0 |
| simd-adler32 | 0.3.9 | MIT |
| slab | 0.4.12 | MIT |
| smallvec | 1.16.2 | MIT OR Apache-2.0 |
| stable_deref_trait | 1.2.1 | MIT OR Apache-2.0 |
| static_assertions | 1.1.0 | MIT OR Apache-2.0 |
| syn | 2.0.118 | MIT OR Apache-2.0 |
| syn | 3.0.5 | MIT OR Apache-2.0 |
| synstructure | 0.14.0 | MIT |
| thiserror | 2.0.20 | MIT OR Apache-2.0 |
| thiserror-impl | 2.0.20 | MIT OR Apache-2.0 |
| tinystr | 0.8.4 | Unicode-3.0 |
| unicode-ident | 1.0.24 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| writeable | 0.6.4 | Unicode-3.0 |
| yoke | 0.8.3 | Unicode-3.0 |
| yoke-derive | 0.8.3 | Unicode-3.0 |
| zerofrom | 0.1.8 | Unicode-3.0 |
| zerofrom-derive | 0.1.8 | Unicode-3.0 |
| zerotrie | 0.2.5 | Unicode-3.0 |
| zerovec | 0.11.8 | Unicode-3.0 |
| zerovec-derive | 0.11.6 | Unicode-3.0 |
| zmij | 1.0.21 | MIT |
| zune-core | 0.5.1 | MIT OR Apache-2.0 OR Zlib |
| zune-jpeg | 0.5.15 | MIT OR Apache-2.0 OR Zlib |

The list is the locked runtime dependency graph of the binary
(`cargo tree --no-default-features -e normal -p plxnative-modules`), excluding build-time and
proc-macro crates (`serde_derive`, `syn`, `quote`, `proc-macro2`, ...), which contribute no code.

**ICU4X code and compiled locale data — Unicode-3.0** (`licenses/ICU4X.txt`): the `icu_*`,
`zerovec`, `yoke`, `tinystr`, `writeable`, `potential_utf`, `litemap` and related crates format
numbers and dates. Copyright © 2020-2024 Unicode, Inc.; portions adapted from ICU4C/ICU4J, ICU 1.8.1
to ICU 57.1 © 1995-2016 International Business Machines Corporation and others.
`calendrical_calculations` implements algorithms from *Calendrical Calculations* by Reingold &
Dershowitz (Apache-2.0). The registry `libm` crate's licence and its musl/CORE-MATH attributions are
in `licenses/libm.txt`.

**compiler_builtins**, declared `MIT AND Apache-2.0 WITH LLVM-exception AND (MIT OR Apache-2.0)`:
`licenses/MIT.txt`, `licenses/Apache-2.0.txt` and `licenses/LLVM-exception.txt` all apply. It
contains code derived from **LLVM's compiler-rt**, "Copyright (c) 2009-2016 by the contributors
listed in CREDITS.TXT". It vendors rust-lang/libm (MIT; `licenses/libm.txt`).

**Unicode Character Database tables in Rust `core`**: Copyright © 1991-2024 Unicode, Inc.
**UNICODE LICENSE V3** (`licenses/Unicode-3.0.txt`).

**Independent JPEG Group acknowledgement.** The `image` crate contains a Rust translation of
`jfdctint.c` from the IJG's libjpeg 9a. *This software is based in part on the work of the
Independent JPEG Group.* IJG code is copyright (C) 1991-2014, Thomas G. Lane, Guido Vollbeding.

### 2.4 Compiler runtime

The C runtime startup objects (`crt1.o`, `crti.o`, `crtn.o`; glibc, LGPL-2.1-or-later) and GCC's
`crtbegin.o`/`crtend.o` (GPL-3.0-or-later **WITH GCC-exception-3.1**,
`licenses/GCC-exception-3.1.txt`) are linked into the executable by the system toolchain.

## 3. System libraries, not distributed

The binary loads these from the operating system at run time; no copy is distributed with PiTV:
GStreamer and its plugins (LGPL-2.1-or-later; the plugins you install decide which codecs are
available), SDL2 and SDL2_ttf (Zlib), GLib (LGPL-2.1-or-later), the GNU C Library
(LGPL-2.1-or-later), zlib (Zlib), and the system's OpenGL/EGL driver (Mesa: MIT).

## 4. The `licenses/` directory

| File | Required by |
|---|---|
| `licenses/LGPL-2.1.txt` | FFmpeg, FriBidi (§1), glibc startup objects (§2.4) |
| `licenses/MIT.txt` | Feather Icons, Heroicons, the MIT Rust crates, compiler_builtins |
| `licenses/Apache-2.0.txt` | Material Design Icons, Apache-2.0 Rust crates, compiler_builtins |
| `licenses/LLVM-exception.txt` | compiler_builtins |
| `licenses/Unicode-3.0.txt` | Unicode tables in Rust `core` |
| `licenses/ICU4X.txt` | ICU4X code and data |
| `licenses/libm.txt` | libm (registry crate and compiler_builtins' vendored copy) |
| `licenses/Zlib.txt` | nanosvg, FreeType's gzip reader, Zlib-licensed crates |
| `licenses/libass-ISC.txt` | libass |
| `licenses/FreeType.txt` | FreeType |
| `licenses/HarfBuzz-Old-MIT.txt` | HarfBuzz |
| `licenses/GCC-exception-3.1.txt` | GCC runtime objects |
| `licenses/PlxNative-historical-MIT.txt` | the original MIT grant of earlier PlxNative versions |

`OFL.txt` (SIL Open Font License 1.1, for Inter) ships beside the fonts.

## Source-only attribution

**Frank Muller** contributed to PlxNative under the then-current MIT grant; that grant and the Gleb
Linnik notice are preserved in `licenses/PlxNative-historical-MIT.txt`.
