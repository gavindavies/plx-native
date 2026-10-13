//! Shared fixtures and helpers for the `ff` test modules split out below.

use super::*;

/// Build a length-prefixed (AVCC-style) packet: 4-byte big-endian length + payload, repeated.
pub(super) fn avcc(nals: &[&[u8]]) -> Vec<u8> {
    let mut v = Vec::new();
    for n in nals {
        v.extend_from_slice(&(n.len() as u32).to_be_bytes());
        v.extend_from_slice(n);
    }
    v
}

pub(super) fn to_annexb(buf: &[u8], is_hevc: bool, param: &[u8]) -> (bool, Vec<u8>) {
    let mut out = Vec::new();
    let key = unsafe { packet_to_annexb(buf.as_ptr(), buf.len(), 4, is_hevc, param, &mut out) };
    (key, out)
}

// -- parse_dovi_conf: the Dolby Vision configuration record ---------------------------
//
// The record is nine plain bytes, so these fixtures ARE the wire format — no builder, no
// FFmpeg. That is the point: `dovi_conf`'s pointer walk cannot be host-tested (dlopen returns
// None on Darwin, so a test naming it would pass without executing one line of it), but the
// parse is where a field could be transposed, and the parse is pure.

/// The nine bytes as the header lays them out, so a fixture reads like the spec table.
pub(super) fn dovi_bytes(profile: u8, level: u8, rpu: u8, el: u8, bl: u8, compat: u8) -> Vec<u8> {
    vec![1, 0, profile, level, rpu, el, bl, compat, 0]
}

// -- the AVIO callbacks under teardown -------------------------------------------------
//
// These drive `read_cb`/`seek_cb` directly, which works on the host precisely because neither
// one touches FFmpeg: they are plain `extern "C"` fns over an `AvioState`, whose every field
// (an HttpStream, an AuQueue, two CStrings, three integers) is ordinary Rust. So long as a
// test stays off `av_*`, the callbacks link and run here exactly as they do on the TV.

// -- the same two invariants, with libcurl under the AVIO instead of a socket ------------
//
// The guards above live in `read_cb`/`seek_cb`, ABOVE the dispatch, so they are transport
// independent by construction — which is exactly the kind of claim that stops being true the
// first time somebody moves a check into a branch. These pin it. The listener speaks plain
// HTTP and curl speaks plain HTTP, so no TLS is needed to grade the abort path; what a host
// cannot reach is a real handshake, which is why the PR carries a device recipe for aborting
// during DNS and during TLS instead of pretending these cover it.

