//! ratch - detachable terminal sessions without terminal emulation.
//!
//! Layers, top to bottom; a module may only use the ones below it:
//!
//! 4. `cli`              argument parsing and commands
//! 3. `client`, `master` the two ends of a session
//! 2. `session`          names, paths, locks and metadata on disk
//! 1. `proto`, `ring`, `vt`  pure logic, no I/O
//! 0. `sys`              the operating system; the only place `unsafe` is allowed
#![deny(unsafe_code)]

pub mod cli;
pub mod client;
pub mod master;
pub mod proto;
pub mod ring;
pub mod session;
pub mod sys;
pub mod vt;
