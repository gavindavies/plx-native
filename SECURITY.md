# Security policy

## Reporting a vulnerability

Report privately through GitHub Security Advisories on this repository (Security → Report a
vulnerability), **not** as a public issue. Please allow a reasonable window for a fix before
disclosing.

## What is in scope

PiTV plays files from local folders and mounted SMB shares. It has no account, no server, no
network client of its own and no telemetry. The things worth looking at:

- **Untrusted media.** Every file it opens is untrusted input: the bundled FFmpeg demuxer
  (`rust-modules/media/src/ff.rs`, demuxers, parsers and subtitle decoders only), the libass facade
  (`src/ass.c`) and the subtitle, `.nfo` and artwork parsers in `rust-modules/data` and
  `rust-modules/media`. Memory-safety bugs reachable through a crafted file are in scope; report
  FFmpeg's, libass's and GStreamer's own bugs upstream as well.
- **The runtime directory.** The event log, the per-file progress and settings live in
  `PITV_RUNTIME_DIR` (`~/.local/state/pitv` via the launcher). Logs are created `0640` with
  `O_NOFOLLOW`. In a build with the `devtriggers` feature, files named `pitv-*` in that directory
  change behaviour and `pitv-remote` is a FIFO that injects key presses — a shipping build is
  compiled with `--no-default-features`, which removes all of it. A shipping binary that still
  honours a trigger or the FIFO is a valid report.

## What is not in scope

GStreamer, SDL, the GL driver, the operating system, and the SMB server or client. Report those to
their maintainers.
