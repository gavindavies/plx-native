#!/bin/sh
# Build the bundled libass facade (pkg/libass-plx-host.so.0) for this machine.
set -eu
exec python3 "$(dirname "$0")/build-libass.py" "$@"
