# PiTV — a media player for the video files on a Linux machine's own disk.
#
#   make            the app, staged into dist/linux/pitv (run dist/linux/pitv/pitv)
#   make check      the host test suite (every crate) and the shipping-feature check
#   make run        build, then open the library in a window
#   make clean      remove the staged app (the cargo tree and the FFmpeg/libass builds stay)
#
# Needs a Rust nightly toolchain, a C compiler, SDL2, SDL2_ttf and GStreamer (with its base and
# good/bad/ugly/libav plugin sets) from the system; see README.md. FFmpeg and libass are built from
# pinned sources into vendor/ the first time and bundled beside the binary, which loads them by
# absolute path.

RUST_NIGHTLY ?= nightly
CARGO        = cd rust-modules && PATH="$$HOME/.cargo/bin:$$PATH" cargo +$(RUST_NIGHTLY)

DIST        ?= dist/linux/pitv
# `DEV=1` keeps the on-screen frame counter and the developer trigger surface (the remote FIFO and
# the `pitv-*` files in the runtime directory); `FEATURES=...` names cargo features outright.
FEATURES    ?= $(if $(DEV),--features devtools$(comma)devtriggers,)
comma       := ,

FFMPEG_PREFIX = vendor/ffmpeg-prefix-host
FFMPEG_LIBS   = libavutil-plx.so.61 libavcodec-plx.so.63 libavformat-plx.so.63 libswscale-plx.so.10
LIBASS_LIB    = pkg/libass-plx-host.so.0
ASSETS        = pkg/appfont.ttf pkg/appfont-bold.ttf pkg/appfont-cjk.ttf pkg/OFL.txt \
                pkg/splash.png pkg/icon.png THIRD-PARTY-NOTICES.md LICENSING.md NOTICE.md

.PHONY: all linux check run clean print-%

all: linux

$(FFMPEG_PREFIX)/lib/libavformat-plx.so.63:
	HOST=1 ./ci/build-ffmpeg.sh

$(LIBASS_LIB): ci/build-libass.sh ci/build-libass.py ci/libass-dependencies.json src/ass.c src/ass_composite.h include/ass.h
	HOST=1 ./ci/build-libass.sh

linux: $(FFMPEG_PREFIX)/lib/libavformat-plx.so.63 $(LIBASS_LIB)
	$(CARGO) build --release --no-default-features $(FEATURES) --bin pitv
	rm -rf $(DIST)
	mkdir -p $(DIST)
	install -m 0755 rust-modules/target/release/pitv $(DIST)/pitv-bin
	install -m 0755 tools/linux/pitv $(DIST)/pitv
	for f in $(FFMPEG_LIBS); do install -m 0644 $(FFMPEG_PREFIX)/lib/$$f $(DIST)/$$f; done
	install -m 0644 $(LIBASS_LIB) $(DIST)/
	install -m 0644 $(ASSETS) $(DIST)/
	@echo "linux: $(DIST)/pitv"

check:
	$(CARGO) test --workspace --lib
	CARGO_INCREMENTAL=0 $(CARGO) check --lib --bins --no-default-features

run: linux
	PITV_FULLSCREEN=0 $(DIST)/pitv

clean:
	rm -rf $(DIST)

print-%:
	@echo $($*)
