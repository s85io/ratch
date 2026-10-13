//! Layer 1: terminal escape sequences. Pure logic, no I/O.

#![forbid(unsafe_code)]

pub mod filter;
pub mod modes;
pub mod parser;

pub use filter::Filter;
pub use modes::Modes;
pub use parser::{Csi, Parser, StrKind, Token};
