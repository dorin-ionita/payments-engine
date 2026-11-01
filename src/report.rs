//! Writing the final account balances as CSV.

use std::io::Write;

use crate::account::Account;
use crate::transaction::ClientId;

/// Writes one row per account, sorted by client id, under the header
/// `client,available,held,total,locked`. Amounts always have four decimals.
///
/// Sorting is not required, but it makes the output identical on every run,
/// which keeps it easy to diff and test. At most 65 536 rows exist.
pub fn write_accounts<'a, W: Write>(
    accounts: impl IntoIterator<Item = (ClientId, &'a Account)>,
    output: W,
) -> csv::Result<()> {
    let mut rows: Vec<_> = accounts.into_iter().collect();
    rows.sort_unstable_by_key(|(client, _)| *client);

    let mut writer = csv::Writer::from_writer(output);
    writer.write_record(["client", "available", "held", "total", "locked"])?;
    for (client, account) in rows {
        writer.write_record([
            client.to_string(),
            account.available().to_string(),
            account.held().to_string(),
            account.total().to_string(),
            account.is_locked().to_string(),
        ])?;
    }
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amount::Amount;
    use crate::engine::Engine;
    use crate::transaction::{Transaction, TxId};

    fn render(engine: &Engine) -> String {
        let mut out = Vec::new();
        write_accounts(engine.accounts(), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn writes_header_only_when_there_are_no_accounts() {
        assert_eq!(
            render(&Engine::new()),
            "client,available,held,total,locked\n"
        );
    }

    #[test]
    fn writes_sorted_rows_with_four_decimals() {
        let mut engine = Engine::new();
        for (client, tx, amount) in [(9, 1, "2"), (3, 2, "1.5"), (3, 3, "0.0001")] {
            engine
                .process(Transaction::Deposit {
                    client: ClientId(client),
                    tx: TxId(tx),
                    amount: amount.parse::<Amount>().unwrap(),
                })
                .unwrap();
        }
        engine
            .process(Transaction::Dispute {
                client: ClientId(9),
                tx: TxId(1),
            })
            .unwrap();
        engine
            .process(Transaction::Chargeback {
                client: ClientId(9),
                tx: TxId(1),
            })
            .unwrap();

        assert_eq!(
            render(&engine),
            "client,available,held,total,locked\n\
             3,1.5001,0.0000,1.5001,false\n\
             9,0.0000,0.0000,0.0000,true\n"
        );
    }
}
