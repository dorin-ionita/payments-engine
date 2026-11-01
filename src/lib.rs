//! A toy payments engine: reads client transactions, maintains account
//! balances, handles disputes, and reports the final state of every account.
//!
//! The library does no I/O of its own; the binary in `main.rs` wires it to
//! CSV input and output.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod amount;

pub use amount::{Amount, ParseAmountError};
