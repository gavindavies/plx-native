//! plx_machine: the deterministic state-machine runtime of the PlxNative application core.
//!
//! Machines and effects (`machine`), the present gate (`present`), the whole-frame idle/wake gate
//! (`idle`), the landing schedule and its record/replay gate (`landing`, `landgate`) and the
//! spring integrators with their own soft-float maths (`motion`). It is the `machine` layer of
//! `ci/module-layers.ini`: it uses `base` and nothing else, and it is not UI, though it lived under
//! `ui/` until the split.
//!
//! `test-support` exposes the `cfg(test)` fixtures other layers' tests build on
//! (`landgate::fixture_gate`, `idle::reset_for_test`, `machine::{BareArg, BareMeasure}`, ...) and
//! keeps this crate's test-mode behaviour (a private wake door per `Present`, the per-thread
//! damage counter) when a dependent's tests build it. The application crate enables it in
//! `[dev-dependencies]` only, so no shipped build sees it.

pub mod machine; // the layer-neutral contract: Host, Machine, Effects, Fx, Canon, Tick (spec §3.1)
pub mod present; // the present gate as a machine with an owner (spec §4.4)
pub mod idle; // whole-FRAME present gating: a screen with nothing moving on it stops repainting
pub mod motion; // the spring integrators' own exp/sin_cos + the soft-float table (spec §4.2)
