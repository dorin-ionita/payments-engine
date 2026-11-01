//! The payments engine: applies transactions to client accounts in order.
//!
//! It keeps two maps: the accounts (at most one per `u16` client id) and a
//! ledger of accepted deposits and withdrawals, which later disputes refer to.
//! Memory therefore grows with the number of accepted transactions, never
//! with the size of the input.
//!
//! A deposit moves through a small dispute lifecycle, all of it in
//! [`Engine::settle`]:
//!
//! ```text
//! Normal --dispute--> Disputed --resolve----> Resolved     (final)
//!                              \--chargeback-> ChargedBack (final, account locked)
//! ```

use std::collections::HashMap;
use std::fmt;

use crate::account::{Account, AccountError};
use crate::amount::Amount;
use crate::error::EngineError;
use crate::transaction::{ClientId, Transaction, TxId};

/// What to do when a dispute arrives for funds the client no longer has
/// available (typically because they were already withdrawn).
///
/// Who carries that loss is a business decision rather than an engine one,
/// so both answers are available; the CLI always uses the default.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum DisputePolicy {
    /// Hold the full amount even if `available` goes negative: the client now
    /// owes the money, and a chargeback can still follow and lock the account.
    #[default]
    AllowNegative,
    /// Refuse the dispute, leaving balances and the transaction untouched.
    RejectIfInsufficient,
}

/// Something worth reporting about a transaction that *was* applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// Assumption 4: a dispute left the client's available balance negative.
    NegativeBalance { client: ClientId, tx: TxId },
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Notice::NegativeBalance { client, tx } => write!(
                f,
                "client {client}: dispute of tx {tx} left available funds negative, \
                 needs follow-up"
            ),
        }
    }
}

#[derive(Debug, Default)]
pub struct Engine {
    accounts: HashMap<ClientId, Account>,
    ledger: HashMap<TxId, StoredTx>,
    policy: DisputePolicy,
}

/// What the engine remembers about an accepted deposit or withdrawal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StoredTx {
    client: ClientId,
    kind: TxKind,
    amount: Amount,
    state: DisputeState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxKind {
    Deposit,
    Withdrawal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisputeState {
    Normal,
    Disputed,
    Resolved,
    ChargedBack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisputeAction {
    Dispute,
    Resolve,
    Chargeback,
}

impl Engine {
    /// An engine with the default [`DisputePolicy`].
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_policy(policy: DisputePolicy) -> Self {
        Engine {
            policy,
            ..Self::default()
        }
    }

    /// Applies one transaction. An error means the transaction was refused and
    /// changed nothing; it is never fatal, so the caller can report it and
    /// carry on. `Ok(Some(_))` means it was applied but deserves attention.
    pub fn process(&mut self, transaction: Transaction) -> Result<Option<Notice>, EngineError> {
        match transaction {
            Transaction::Deposit { client, tx, amount } => {
                self.move_funds(client, tx, amount, TxKind::Deposit, Account::deposit)
            }
            Transaction::Withdrawal { client, tx, amount } => {
                self.move_funds(client, tx, amount, TxKind::Withdrawal, Account::withdraw)
            }
            Transaction::Dispute { client, tx } => self.settle(DisputeAction::Dispute, client, tx),
            Transaction::Resolve { client, tx } => self.settle(DisputeAction::Resolve, client, tx),
            Transaction::Chargeback { client, tx } => {
                self.settle(DisputeAction::Chargeback, client, tx)
            }
        }
    }

    /// Every known account, in no particular order.
    pub fn accounts(&self) -> impl Iterator<Item = (ClientId, &Account)> {
        self.accounts
            .iter()
            .map(|(client, account)| (*client, account))
    }

    /// Shared path for deposits and withdrawals: apply `operation` to the
    /// client's account and, only if it succeeds, record the transaction so
    /// it can be disputed later. Ids of refused transactions are not kept.
    fn move_funds(
        &mut self,
        client: ClientId,
        tx: TxId,
        amount: Amount,
        kind: TxKind,
        operation: fn(&mut Account, Amount) -> Result<(), AccountError>,
    ) -> Result<Option<Notice>, EngineError> {
        // Assumption 7: any deposit or withdrawal row creates the client,
        // even when the transaction itself is refused below.
        let account = self.accounts.entry(client).or_default();

        // Assumption 6: transaction ids are unique; the first one wins.
        if self.ledger.contains_key(&tx) {
            return Err(EngineError::DuplicateTx { tx });
        }
        operation(account, amount).map_err(|err| EngineError::from_account(err, client, tx))?;
        self.ledger.insert(
            tx,
            StoredTx {
                client,
                kind,
                amount,
                state: DisputeState::Normal,
            },
        );
        Ok(None)
    }

    /// Dispute, resolve or charge back an earlier deposit. Every check runs
    /// before any balance changes, so a refusal leaves everything untouched.
    fn settle(
        &mut self,
        action: DisputeAction,
        client: ClientId,
        tx: TxId,
    ) -> Result<Option<Notice>, EngineError> {
        // Unknown references never create a client.
        let stored = self
            .ledger
            .get_mut(&tx)
            .ok_or(EngineError::UnknownTx { tx })?;
        // Assumption 2: clients can only act on their own transactions.
        if stored.client != client {
            return Err(EngineError::ClientMismatch { client, tx });
        }
        // Assumption 1: only deposits can be disputed.
        if stored.kind == TxKind::Withdrawal {
            return Err(EngineError::UnsupportedDispute { tx });
        }
        // The deposit created this account, so it always exists.
        let account = self
            .accounts
            .get_mut(&client)
            .ok_or(EngineError::UnknownTx { tx })?;
        // Assumption 3: a locked account accepts nothing, including the
        // settlement of disputes that were already open.
        if account.is_locked() {
            return Err(EngineError::AccountLocked { client, tx });
        }

        // The whole lifecycle in one place. Assumption 5: Resolved and
        // ChargedBack are final; a second dispute needs a human.
        let next_state = match (action, stored.state) {
            (DisputeAction::Dispute, DisputeState::Normal) => DisputeState::Disputed,
            (
                DisputeAction::Dispute,
                DisputeState::Disputed | DisputeState::Resolved | DisputeState::ChargedBack,
            ) => return Err(EngineError::AlreadyDisputed { tx }),
            (DisputeAction::Resolve, DisputeState::Disputed) => DisputeState::Resolved,
            (DisputeAction::Chargeback, DisputeState::Disputed) => DisputeState::ChargedBack,
            (
                DisputeAction::Resolve | DisputeAction::Chargeback,
                DisputeState::Normal | DisputeState::Resolved | DisputeState::ChargedBack,
            ) => return Err(EngineError::NotDisputed { tx }),
        };

        let amount = stored.amount;
        let result = match action {
            DisputeAction::Dispute => {
                // Assumption 4: by default a dispute may drive available
                // negative; the stricter policy refuses it instead.
                if self.policy == DisputePolicy::RejectIfInsufficient
                    && account.available() < amount
                {
                    return Err(EngineError::InsufficientFundsForDispute { client, tx });
                }
                account.hold(amount)
            }
            DisputeAction::Resolve => account.release(amount),
            DisputeAction::Chargeback => account.charge_back(amount),
        };
        result.map_err(|err| EngineError::from_account(err, client, tx))?;
        stored.state = next_state;

        let went_negative = action == DisputeAction::Dispute && account.available() < Amount::ZERO;
        Ok(went_negative.then_some(Notice::NegativeBalance { client, tx }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: ClientId = ClientId(1);
    const BOB: ClientId = ClientId(2);

    fn amt(s: &str) -> Amount {
        s.parse().unwrap()
    }

    fn deposit(client: ClientId, tx: u32, amount: &str) -> Transaction {
        Transaction::Deposit {
            client,
            tx: TxId(tx),
            amount: amt(amount),
        }
    }

    fn withdrawal(client: ClientId, tx: u32, amount: &str) -> Transaction {
        Transaction::Withdrawal {
            client,
            tx: TxId(tx),
            amount: amt(amount),
        }
    }

    fn dispute(client: ClientId, tx: u32) -> Transaction {
        Transaction::Dispute {
            client,
            tx: TxId(tx),
        }
    }

    fn resolve(client: ClientId, tx: u32) -> Transaction {
        Transaction::Resolve {
            client,
            tx: TxId(tx),
        }
    }

    fn chargeback(client: ClientId, tx: u32) -> Transaction {
        Transaction::Chargeback {
            client,
            tx: TxId(tx),
        }
    }

    /// Applies transactions that are all expected to succeed.
    fn run(engine: &mut Engine, transactions: &[Transaction]) {
        for transaction in transactions {
            engine
                .process(*transaction)
                .unwrap_or_else(|err| panic!("{transaction:?} was refused: {err}"));
        }
    }

    /// (available, held, total, locked) of `client`, or `None` if unknown.
    fn balances(engine: &Engine, client: ClientId) -> Option<(Amount, Amount, Amount, bool)> {
        engine
            .accounts()
            .find(|(id, _)| *id == client)
            .map(|(_, account)| {
                (
                    account.available(),
                    account.held(),
                    account.total(),
                    account.is_locked(),
                )
            })
    }

    fn expect(
        available: &str,
        held: &str,
        total: &str,
        locked: bool,
    ) -> Option<(Amount, Amount, Amount, bool)> {
        Some((amt(available), amt(held), amt(total), locked))
    }

    // --- deposits and withdrawals -------------------------------------------

    #[test]
    fn deposit_creates_the_client_and_credits_it() {
        let mut engine = Engine::new();
        run(&mut engine, &[deposit(ALICE, 1, "1.5")]);
        assert_eq!(balances(&engine, ALICE), expect("1.5", "0", "1.5", false));
        assert_eq!(balances(&engine, BOB), None);
    }

    #[test]
    fn clients_are_independent() {
        let mut engine = Engine::new();
        run(
            &mut engine,
            &[
                deposit(BOB, 7, "2"),
                deposit(ALICE, 3, "1"),
                deposit(ALICE, 5, "2"),
                withdrawal(ALICE, 4, "1.5"),
            ],
        );
        assert_eq!(balances(&engine, ALICE), expect("1.5", "0", "1.5", false));
        assert_eq!(balances(&engine, BOB), expect("2", "0", "2", false));
    }

    #[test]
    fn withdrawal_with_insufficient_funds_is_refused() {
        let mut engine = Engine::new();
        run(&mut engine, &[deposit(ALICE, 1, "1")]);
        assert_eq!(
            engine.process(withdrawal(ALICE, 2, "3")),
            Err(EngineError::InsufficientFunds {
                client: ALICE,
                tx: TxId(2)
            })
        );
        assert_eq!(balances(&engine, ALICE), expect("1", "0", "1", false));
    }

    #[test]
    fn refused_withdrawal_still_creates_the_client() {
        let mut engine = Engine::new();
        assert!(engine.process(withdrawal(BOB, 1, "1")).is_err());
        assert_eq!(balances(&engine, BOB), expect("0", "0", "0", false));
    }

    #[test]
    fn duplicate_transaction_id_is_refused_and_first_wins() {
        let mut engine = Engine::new();
        run(&mut engine, &[deposit(ALICE, 1, "1")]);
        let duplicate = Err(EngineError::DuplicateTx { tx: TxId(1) });
        assert_eq!(engine.process(deposit(ALICE, 1, "5")), duplicate);
        assert_eq!(engine.process(withdrawal(ALICE, 1, "1")), duplicate);
        assert_eq!(balances(&engine, ALICE), expect("1", "0", "1", false));
    }

    #[test]
    fn ids_of_refused_transactions_can_be_used_again() {
        let mut engine = Engine::new();
        assert!(engine.process(withdrawal(ALICE, 1, "1")).is_err());
        run(&mut engine, &[deposit(ALICE, 1, "2")]);
        assert_eq!(balances(&engine, ALICE), expect("2", "0", "2", false));
    }

    #[test]
    fn overflowing_deposit_is_refused() {
        let mut engine = Engine::new();
        engine
            .process(Transaction::Deposit {
                client: ALICE,
                tx: TxId(1),
                amount: Amount::from_units(i64::MAX),
            })
            .unwrap();
        assert_eq!(
            engine.process(deposit(ALICE, 2, "0.0001")),
            Err(EngineError::Overflow {
                client: ALICE,
                tx: TxId(2)
            })
        );
    }

    // --- the dispute lifecycle ----------------------------------------------

    #[test]
    fn dispute_moves_funds_to_held() {
        let mut engine = Engine::new();
        run(
            &mut engine,
            &[
                deposit(ALICE, 1, "5"),
                deposit(ALICE, 2, "3"),
                dispute(ALICE, 1),
            ],
        );
        assert_eq!(balances(&engine, ALICE), expect("3", "5", "8", false));
    }

    #[test]
    fn resolve_releases_held_funds() {
        let mut engine = Engine::new();
        run(
            &mut engine,
            &[deposit(ALICE, 1, "5"), dispute(ALICE, 1), resolve(ALICE, 1)],
        );
        assert_eq!(balances(&engine, ALICE), expect("5", "0", "5", false));
    }

    #[test]
    fn chargeback_removes_held_funds_and_locks_the_account() {
        let mut engine = Engine::new();
        run(
            &mut engine,
            &[
                deposit(ALICE, 1, "5"),
                deposit(ALICE, 2, "3"),
                dispute(ALICE, 1),
                chargeback(ALICE, 1),
            ],
        );
        assert_eq!(balances(&engine, ALICE), expect("3", "0", "3", true));
    }

    #[test]
    fn dispute_amount_comes_from_the_referenced_deposit() {
        let mut engine = Engine::new();
        run(
            &mut engine,
            &[
                deposit(ALICE, 1, "1.2345"),
                deposit(ALICE, 2, "10"),
                dispute(ALICE, 1),
            ],
        );
        assert_eq!(
            balances(&engine, ALICE),
            expect("10", "1.2345", "11.2345", false)
        );
    }

    #[test]
    fn resolved_transaction_cannot_be_disputed_again() {
        let mut engine = Engine::new();
        run(
            &mut engine,
            &[deposit(ALICE, 1, "5"), dispute(ALICE, 1), resolve(ALICE, 1)],
        );
        assert_eq!(
            engine.process(dispute(ALICE, 1)),
            Err(EngineError::AlreadyDisputed { tx: TxId(1) })
        );
        assert_eq!(balances(&engine, ALICE), expect("5", "0", "5", false));
    }

    #[test]
    fn open_dispute_cannot_be_disputed_again() {
        let mut engine = Engine::new();
        run(&mut engine, &[deposit(ALICE, 1, "5"), dispute(ALICE, 1)]);
        assert_eq!(
            engine.process(dispute(ALICE, 1)),
            Err(EngineError::AlreadyDisputed { tx: TxId(1) })
        );
        assert_eq!(balances(&engine, ALICE), expect("0", "5", "5", false));
    }

    #[test]
    fn resolve_and_chargeback_need_an_open_dispute() {
        let mut engine = Engine::new();
        run(&mut engine, &[deposit(ALICE, 1, "5")]);
        let not_disputed = Err(EngineError::NotDisputed { tx: TxId(1) });
        assert_eq!(engine.process(resolve(ALICE, 1)), not_disputed);
        assert_eq!(engine.process(chargeback(ALICE, 1)), not_disputed);

        run(&mut engine, &[dispute(ALICE, 1), resolve(ALICE, 1)]);
        assert_eq!(engine.process(resolve(ALICE, 1)), not_disputed);
        assert_eq!(engine.process(chargeback(ALICE, 1)), not_disputed);
        assert_eq!(balances(&engine, ALICE), expect("5", "0", "5", false));
    }

    #[test]
    fn unknown_transaction_is_refused_without_creating_a_client() {
        let mut engine = Engine::new();
        for transaction in [dispute(BOB, 9), resolve(BOB, 9), chargeback(BOB, 9)] {
            assert_eq!(
                engine.process(transaction),
                Err(EngineError::UnknownTx { tx: TxId(9) })
            );
        }
        assert_eq!(balances(&engine, BOB), None);
    }

    #[test]
    fn withdrawals_cannot_be_disputed() {
        let mut engine = Engine::new();
        run(
            &mut engine,
            &[deposit(ALICE, 1, "5"), withdrawal(ALICE, 2, "2")],
        );
        for transaction in [dispute(ALICE, 2), resolve(ALICE, 2), chargeback(ALICE, 2)] {
            assert_eq!(
                engine.process(transaction),
                Err(EngineError::UnsupportedDispute { tx: TxId(2) })
            );
        }
        assert_eq!(balances(&engine, ALICE), expect("3", "0", "3", false));
    }

    #[test]
    fn clients_cannot_act_on_other_clients_transactions() {
        let mut engine = Engine::new();
        run(&mut engine, &[deposit(ALICE, 1, "5"), deposit(BOB, 2, "1")]);
        let mismatch = Err(EngineError::ClientMismatch {
            client: BOB,
            tx: TxId(1),
        });
        assert_eq!(engine.process(dispute(BOB, 1)), mismatch);

        run(&mut engine, &[dispute(ALICE, 1)]);
        assert_eq!(engine.process(resolve(BOB, 1)), mismatch);
        assert_eq!(engine.process(chargeback(BOB, 1)), mismatch);
        assert_eq!(balances(&engine, ALICE), expect("0", "5", "5", false));
        assert_eq!(balances(&engine, BOB), expect("1", "0", "1", false));
    }

    // --- locked accounts ----------------------------------------------------

    fn locked_engine() -> Engine {
        let mut engine = Engine::new();
        run(
            &mut engine,
            &[
                deposit(ALICE, 1, "5"),
                deposit(ALICE, 2, "3"),
                dispute(ALICE, 1),
                chargeback(ALICE, 1),
            ],
        );
        engine
    }

    #[test]
    fn locked_account_rejects_deposits_and_withdrawals() {
        let mut engine = locked_engine();
        let locked = |tx| {
            Err(EngineError::AccountLocked {
                client: ALICE,
                tx: TxId(tx),
            })
        };
        assert_eq!(engine.process(deposit(ALICE, 10, "1")), locked(10));
        assert_eq!(engine.process(withdrawal(ALICE, 11, "1")), locked(11));
        assert_eq!(balances(&engine, ALICE), expect("3", "0", "3", true));
    }

    #[test]
    fn locked_account_rejects_new_disputes() {
        let mut engine = locked_engine();
        let locked = |tx| {
            Err(EngineError::AccountLocked {
                client: ALICE,
                tx: TxId(tx),
            })
        };
        assert_eq!(engine.process(dispute(ALICE, 2)), locked(2));
        assert_eq!(engine.process(dispute(ALICE, 1)), locked(1));
        assert_eq!(balances(&engine, ALICE), expect("3", "0", "3", true));
    }

    #[test]
    fn locked_account_keeps_open_disputes_held() {
        // tx 2 is still disputed when tx 1's chargeback locks the account:
        // its funds stay held until a human settles it.
        let mut engine = Engine::new();
        run(
            &mut engine,
            &[
                deposit(ALICE, 1, "5"),
                deposit(ALICE, 2, "3"),
                dispute(ALICE, 1),
                dispute(ALICE, 2),
                chargeback(ALICE, 1),
            ],
        );
        let locked = Err(EngineError::AccountLocked {
            client: ALICE,
            tx: TxId(2),
        });
        assert_eq!(engine.process(resolve(ALICE, 2)), locked);
        assert_eq!(engine.process(chargeback(ALICE, 2)), locked);
        assert_eq!(balances(&engine, ALICE), expect("0", "3", "3", true));
    }

    // --- disputing funds that were already withdrawn ------------------------

    #[test]
    fn dispute_after_withdrawal_drives_available_negative_by_default() {
        let mut engine = Engine::new();
        run(
            &mut engine,
            &[deposit(ALICE, 1, "10"), withdrawal(ALICE, 2, "8")],
        );
        assert_eq!(
            engine.process(dispute(ALICE, 1)),
            Ok(Some(Notice::NegativeBalance {
                client: ALICE,
                tx: TxId(1)
            }))
        );
        assert_eq!(balances(&engine, ALICE), expect("-8", "10", "2", false));

        run(&mut engine, &[chargeback(ALICE, 1)]);
        assert_eq!(balances(&engine, ALICE), expect("-8", "0", "-8", true));
    }

    #[test]
    fn dispute_that_keeps_available_non_negative_has_no_notice() {
        let mut engine = Engine::new();
        run(
            &mut engine,
            &[
                deposit(ALICE, 1, "10"),
                withdrawal(ALICE, 2, "4"),
                deposit(ALICE, 3, "5"),
            ],
        );
        assert_eq!(engine.process(dispute(ALICE, 1)), Ok(None));
        assert_eq!(balances(&engine, ALICE), expect("1", "10", "11", false));
    }

    #[test]
    fn strict_policy_refuses_dispute_without_enough_available() {
        let mut engine = Engine::with_policy(DisputePolicy::RejectIfInsufficient);
        run(
            &mut engine,
            &[deposit(ALICE, 1, "10"), withdrawal(ALICE, 2, "8")],
        );
        assert_eq!(
            engine.process(dispute(ALICE, 1)),
            Err(EngineError::InsufficientFundsForDispute {
                client: ALICE,
                tx: TxId(1)
            })
        );
        assert_eq!(balances(&engine, ALICE), expect("2", "0", "2", false));

        // The refused dispute left the deposit disputable once funds return.
        run(&mut engine, &[deposit(ALICE, 3, "8"), dispute(ALICE, 1)]);
        assert_eq!(balances(&engine, ALICE), expect("0", "10", "10", false));
    }

    #[test]
    fn default_policy_is_allow_negative() {
        assert_eq!(DisputePolicy::default(), DisputePolicy::AllowNegative);
    }
}
