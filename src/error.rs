//! Every reason the engine can refuse a transaction.
//!
//! Together the variants form the catalogue of business rules. None of them
//! is fatal: the caller reports the error and moves on to the next
//! transaction, and the refused transaction changes no balance.

use crate::account::AccountError;
use crate::transaction::{ClientId, TxId};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// Assumption 3: a locked account accepts nothing.
    #[error("client {client}: account is locked, tx {tx} ignored")]
    AccountLocked { client: ClientId, tx: TxId },

    /// Assumption 6: transaction ids are unique; the first one wins.
    #[error("tx {tx}: duplicate transaction id, ignored")]
    DuplicateTx { tx: TxId },

    #[error("client {client}: insufficient available funds for tx {tx}")]
    InsufficientFunds { client: ClientId, tx: TxId },

    #[error("client {client}: tx {tx} would overflow the balance, ignored")]
    Overflow { client: ClientId, tx: TxId },

    #[error("tx {tx}: unknown transaction, ignored")]
    UnknownTx { tx: TxId },

    /// Assumption 2: clients can only act on their own transactions.
    #[error("client {client}: tx {tx} belongs to another client, ignored")]
    ClientMismatch { client: ClientId, tx: TxId },

    /// Assumption 1: only deposits can be disputed.
    #[error("tx {tx}: is a withdrawal; disputes of withdrawals are not supported, ignored")]
    UnsupportedDispute { tx: TxId },

    /// Assumption 5: a transaction can be disputed only once.
    #[error("tx {tx}: already disputed, needs manual review")]
    AlreadyDisputed { tx: TxId },

    #[error("tx {tx}: not under dispute, ignored")]
    NotDisputed { tx: TxId },

    /// Only under `DisputePolicy::RejectIfInsufficient` (assumption 4).
    #[error("client {client}: not enough available funds to dispute tx {tx}, ignored")]
    InsufficientFundsForDispute { client: ClientId, tx: TxId },

    /// Should be unreachable: the dispute state machine only releases or
    /// charges back funds it previously held.
    #[error("client {client}: held funds lower than tx {tx} amount (internal error)")]
    HeldMismatch { client: ClientId, tx: TxId },
}

impl EngineError {
    /// Adds the client and transaction context to an account-level error.
    pub(crate) fn from_account(err: AccountError, client: ClientId, tx: TxId) -> Self {
        match err {
            AccountError::Locked => EngineError::AccountLocked { client, tx },
            AccountError::InsufficientFunds => EngineError::InsufficientFunds { client, tx },
            AccountError::Overflow => EngineError::Overflow { client, tx },
            AccountError::InsufficientHeld => EngineError::HeldMismatch { client, tx },
        }
    }
}
