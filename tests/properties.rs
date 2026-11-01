//! Property tests: random transaction sequences, with the engine's
//! invariants checked after every single step, under both dispute policies.
//!
//! The test keeps its own simple model of what each client's total and held
//! funds should be, independent of the engine's code, and compares it with
//! the engine after every transaction.

use std::collections::{HashMap, HashSet};

use payments_engine::{
    csv_reader, Account, Amount, ClientId, DisputePolicy, Engine, Transaction, TxId,
};
use proptest::prelude::*;

/// Few clients and tx ids, so that random sequences often collide: disputes
/// hit real deposits, ids repeat, clients get locked.
fn transaction() -> impl Strategy<Value = Transaction> {
    let client = || (1u16..=3).prop_map(ClientId);
    let tx = || (1u32..=20).prop_map(TxId);
    let amount = || (1i64..=1_000_000).prop_map(Amount::from_units);
    prop_oneof![
        3 => (client(), tx(), amount())
            .prop_map(|(client, tx, amount)| Transaction::Deposit { client, tx, amount }),
        2 => (client(), tx(), amount())
            .prop_map(|(client, tx, amount)| Transaction::Withdrawal { client, tx, amount }),
        2 => (client(), tx()).prop_map(|(client, tx)| Transaction::Dispute { client, tx }),
        1 => (client(), tx()).prop_map(|(client, tx)| Transaction::Resolve { client, tx }),
        1 => (client(), tx()).prop_map(|(client, tx)| Transaction::Chargeback { client, tx }),
    ]
}

/// What the balances should be, tracked independently of the engine.
#[derive(Default)]
struct Model {
    /// Accepted deposits, which are the only disputable transactions.
    deposits: HashMap<TxId, (ClientId, Amount)>,
    /// Deposits currently under dispute.
    open_disputes: HashSet<TxId>,
    /// Expected total per client: deposits - withdrawals - chargebacks.
    totals: HashMap<ClientId, Amount>,
    locked: HashSet<ClientId>,
}

impl Model {
    fn accept(&mut self, transaction: Transaction) {
        let add = |totals: &mut HashMap<ClientId, Amount>, client, amount: Amount, sign: i8| {
            let total = totals.entry(client).or_default();
            *total = if sign > 0 {
                total.checked_add(amount).unwrap()
            } else {
                total.checked_sub(amount).unwrap()
            };
        };
        match transaction {
            Transaction::Deposit { client, tx, amount } => {
                self.deposits.insert(tx, (client, amount));
                add(&mut self.totals, client, amount, 1);
            }
            Transaction::Withdrawal { client, amount, .. } => {
                add(&mut self.totals, client, amount, -1);
            }
            Transaction::Dispute { tx, .. } => {
                self.open_disputes.insert(tx);
            }
            Transaction::Resolve { tx, .. } => {
                self.open_disputes.remove(&tx);
            }
            Transaction::Chargeback { client, tx } => {
                self.open_disputes.remove(&tx);
                let (_, amount) = self.deposits[&tx];
                add(&mut self.totals, client, amount, -1);
                self.locked.insert(client);
            }
        }
    }

    fn held(&self, client: ClientId) -> Amount {
        self.open_disputes
            .iter()
            .map(|tx| self.deposits[tx])
            .filter(|(owner, _)| *owner == client)
            .fold(Amount::ZERO, |sum, (_, amount)| {
                sum.checked_add(amount).unwrap()
            })
    }
}

fn snapshot(engine: &Engine) -> HashMap<ClientId, Account> {
    engine
        .accounts()
        .map(|(client, account)| (client, account.clone()))
        .collect()
}

fn check_sequence(
    policy: DisputePolicy,
    transactions: &[Transaction],
) -> Result<(), TestCaseError> {
    let mut engine = Engine::with_policy(policy);
    let mut model = Model::default();

    for &transaction in transactions {
        let before = snapshot(&engine);
        let result = engine.process(transaction);
        let after = snapshot(&engine);

        match result {
            Ok(_) => {
                model.accept(transaction);
                if let Transaction::Withdrawal { client, .. } = transaction {
                    // A withdrawal never makes available negative, under any policy.
                    prop_assert!(after[&client].available() >= Amount::ZERO);
                }
            }
            Err(_) => {
                // A refusal changes no existing account. The only allowed
                // side effect is a new, empty account (assumption 7).
                for (client, account) in &after {
                    match before.get(client) {
                        Some(previous) => prop_assert_eq!(previous, account),
                        None => {
                            let moves_funds = matches!(
                                transaction,
                                Transaction::Deposit { .. } | Transaction::Withdrawal { .. }
                            );
                            prop_assert!(moves_funds);
                            prop_assert_eq!(account, &Account::default());
                        }
                    }
                }
            }
        }

        for (client, account) in &after {
            // A locked account never changes again.
            if let Some(previous) = before.get(client).filter(|a| a.is_locked()) {
                prop_assert_eq!(previous, account);
            }
            prop_assert_eq!(account.is_locked(), model.locked.contains(client));
            prop_assert!(account.held() >= Amount::ZERO);
            prop_assert_eq!(
                Some(account.total()),
                account.available().checked_add(account.held())
            );
            // Money is conserved, and held is exactly the open disputes.
            let expected_total = model.totals.get(client).copied().unwrap_or_default();
            prop_assert_eq!(account.total(), expected_total);
            prop_assert_eq!(account.held(), model.held(*client));
            if policy == DisputePolicy::RejectIfInsufficient {
                prop_assert!(account.available() >= Amount::ZERO);
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn invariants_hold_with_default_policy(
        transactions in prop::collection::vec(transaction(), 0..120)
    ) {
        check_sequence(DisputePolicy::AllowNegative, &transactions)?;
    }

    #[test]
    fn invariants_hold_with_strict_policy(
        transactions in prop::collection::vec(transaction(), 0..120)
    ) {
        check_sequence(DisputePolicy::RejectIfInsufficient, &transactions)?;
    }

    /// Every value prints and parses back to itself. (`i64::MIN` is the one
    /// exception: its magnitude does not fit in an i64 while parsing.)
    #[test]
    fn amount_display_round_trips(units in (i64::MIN + 1)..=i64::MAX) {
        let amount = Amount::from_units(units);
        prop_assert_eq!(amount.to_string().parse::<Amount>(), Ok(amount));
    }

    /// Arbitrary text never makes amount parsing panic.
    #[test]
    fn amount_parsing_never_panics(input in "[-+.0-9 a-z\\t]{0,30}") {
        let _ = input.parse::<Amount>();
    }

    /// Arbitrary rows never make transaction parsing panic.
    #[test]
    fn row_parsing_never_panics(row in "\\PC{0,60}") {
        let input = format!("type,client,tx,amount\n{row}\n");
        let mut reader = csv_reader(input.as_bytes());
        let headers = reader.headers().unwrap().clone();
        for record in reader.records().flatten() {
            let _ = Transaction::from_record(&record, &headers);
        }
    }
}
