//! Deterministic, procedurally generated Skyrim asset fixtures.
//!
//! Every byte produced by this crate is synthesized from a caller-provided
//! seed. No proprietary game data is read, copied, or required, which lets
//! contributors exercise the converter and runtime without a local game
//! installation.
#![forbid(unsafe_code)]

pub mod rng;
