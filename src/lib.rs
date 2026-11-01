//! A toy payments engine: reads client transactions, maintains account
//! balances, handles disputes, and reports the final state of every account.
//!
//! The library does no I/O of its own; the binary in `main.rs` wires it to
//! CSV input and output.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod account;
pub mod amount;
pub mod engine;
pub mod error;
pub mod report;
pub mod transaction;

pub use account::{Account, AccountError};
pub use amount::{Amount, ParseAmountError};
pub use engine::{DisputePolicy, Engine, Notice};
pub use error::EngineError;
pub use transaction::{csv_reader, ClientId, ParseError, Transaction, TxId};
