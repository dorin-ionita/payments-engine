//! Turning CSV rows into validated [`Transaction`]s.
//!
//! This is the parsing boundary: a row either becomes a fully valid
//! `Transaction` or a [`ParseError`]. Code past this point never re-checks
//! amounts, and dispute-style transactions carry no amount at all, so an
//! amount written on such a row can never be used by mistake.

use std::fmt;
use std::io::Read;

use csv::{ReaderBuilder, StringRecord, Trim};
use serde::Deserialize;

use crate::amount::{Amount, ParseAmountError};

/// A client identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(pub u16);

/// A globally unique transaction identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TxId(pub u32);

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for TxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One validated input row. Deposit and withdrawal amounts are always > 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transaction {
    /// Credits the client's available funds.
    Deposit {
        client: ClientId,
        tx: TxId,
        amount: Amount,
    },
    /// Debits the client's available funds, if they suffice.
    Withdrawal {
        client: ClientId,
        tx: TxId,
        amount: Amount,
    },
    /// Claims that deposit `tx` was erroneous: its amount is held.
    Dispute { client: ClientId, tx: TxId },
    /// Closes the dispute on `tx` in the client's favour: the amount is released.
    Resolve { client: ClientId, tx: TxId },
    /// Closes the dispute on `tx` by reversing it: the amount is removed and
    /// the account locked.
    Chargeback { client: ClientId, tx: TxId },
}

/// Why a row was rejected before reaching the engine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// Wrong shape: missing columns, or a client or tx id that isn't a valid
    /// `u16` / `u32`.
    #[error("malformed row: {0}")]
    Malformed(String),
    /// A `type` other than the five known ones (matched exactly, lowercase).
    #[error("unknown transaction type {0:?}")]
    UnknownType(String),
    /// A deposit or withdrawal without an amount.
    #[error("missing amount")]
    MissingAmount,
    /// A deposit or withdrawal amount that isn't a valid [`Amount`].
    #[error("invalid amount: {0}")]
    InvalidAmount(#[from] ParseAmountError),
    /// A deposit or withdrawal amount of zero or less.
    #[error("amount must be greater than zero")]
    NonPositiveAmount,
}

/// The raw shape of a row. Fields borrow from the record, so reading a row
/// allocates nothing.
#[derive(Deserialize)]
struct RawRecord<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    client: u16,
    tx: u32,
    /// Absent or empty for dispute, resolve and chargeback rows.
    #[serde(default)]
    amount: Option<&'a str>,
}

/// A CSV reader configured for transaction input: a header row, whitespace
/// trimmed around every header and field, and rows allowed to omit the
/// trailing amount column.
pub fn csv_reader<R: Read>(input: R) -> csv::Reader<R> {
    ReaderBuilder::new()
        .trim(Trim::All)
        .flexible(true)
        .from_reader(input)
}

impl Transaction {
    /// Validates one row. Columns are matched by header name, so their order
    /// in the input does not matter.
    pub fn from_record(record: &StringRecord, headers: &StringRecord) -> Result<Self, ParseError> {
        let raw: RawRecord = record
            .deserialize(Some(headers))
            .map_err(|err| ParseError::Malformed(describe(&err)))?;
        let client = ClientId(raw.client);
        let tx = TxId(raw.tx);

        Ok(match raw.kind {
            "deposit" => Transaction::Deposit {
                client,
                tx,
                amount: positive_amount(raw.amount)?,
            },
            "withdrawal" => Transaction::Withdrawal {
                client,
                tx,
                amount: positive_amount(raw.amount)?,
            },
            // Any amount on these rows is ignored: the engine uses the amount
            // of the referenced transaction.
            "dispute" => Transaction::Dispute { client, tx },
            "resolve" => Transaction::Resolve { client, tx },
            "chargeback" => Transaction::Chargeback { client, tx },
            other => return Err(ParseError::UnknownType(other.to_owned())),
        })
    }

    /// The client the row names (for routing, e.g. to a shard).
    pub fn client(&self) -> ClientId {
        match *self {
            Transaction::Deposit { client, .. }
            | Transaction::Withdrawal { client, .. }
            | Transaction::Dispute { client, .. }
            | Transaction::Resolve { client, .. }
            | Transaction::Chargeback { client, .. } => client,
        }
    }

    /// The transaction id: its own for deposits and withdrawals, the
    /// referenced deposit's for disputes, resolves and chargebacks.
    pub fn tx(&self) -> TxId {
        match *self {
            Transaction::Deposit { tx, .. }
            | Transaction::Withdrawal { tx, .. }
            | Transaction::Dispute { tx, .. }
            | Transaction::Resolve { tx, .. }
            | Transaction::Chargeback { tx, .. } => tx,
        }
    }
}

fn positive_amount(raw: Option<&str>) -> Result<Amount, ParseError> {
    let amount: Amount = raw.ok_or(ParseError::MissingAmount)?.parse()?;
    if amount <= Amount::ZERO {
        return Err(ParseError::NonPositiveAmount);
    }
    Ok(amount)
}

/// The useful part of a csv error, without the position (the caller reports
/// the line number itself).
fn describe(err: &csv::Error) -> String {
    match err.kind() {
        csv::ErrorKind::Deserialize { err, .. } => err.to_string(),
        _ => err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses every data row of `input` (header included in `input`).
    fn parse_rows(input: &str) -> Vec<Result<Transaction, ParseError>> {
        let mut reader = csv_reader(input.as_bytes());
        let headers = reader.headers().unwrap().clone();
        reader
            .records()
            .map(|record| Transaction::from_record(&record.unwrap(), &headers))
            .collect()
    }

    fn parse_one(row: &str) -> Result<Transaction, ParseError> {
        let mut rows = parse_rows(&format!("type,client,tx,amount\n{row}\n"));
        assert_eq!(rows.len(), 1, "expected exactly one row");
        rows.remove(0)
    }

    fn amt(s: &str) -> Amount {
        s.parse().unwrap()
    }

    #[test]
    fn parses_every_transaction_type() {
        let (client, tx) = (ClientId(7), TxId(42));
        assert_eq!(
            parse_one("deposit,7,42,1.5"),
            Ok(Transaction::Deposit {
                client,
                tx,
                amount: amt("1.5")
            })
        );
        assert_eq!(
            parse_one("withdrawal,7,42,0.25"),
            Ok(Transaction::Withdrawal {
                client,
                tx,
                amount: amt("0.25")
            })
        );
        assert_eq!(
            parse_one("dispute,7,42,"),
            Ok(Transaction::Dispute { client, tx })
        );
        assert_eq!(
            parse_one("resolve,7,42,"),
            Ok(Transaction::Resolve { client, tx })
        );
        assert_eq!(
            parse_one("chargeback,7,42,"),
            Ok(Transaction::Chargeback { client, tx })
        );
    }

    #[test]
    fn trims_whitespace_in_headers_and_fields() {
        let rows = parse_rows("type , client,  tx, amount\n  deposit ,  3 , 9 ,  2.5  \n");
        assert_eq!(
            rows,
            vec![Ok(Transaction::Deposit {
                client: ClientId(3),
                tx: TxId(9),
                amount: amt("2.5"),
            })]
        );
    }

    #[test]
    fn dispute_rows_may_omit_the_amount_column() {
        let rows = parse_rows("type,client,tx,amount\ndispute,1,2\nresolve, 1, 2\n");
        assert_eq!(
            rows,
            vec![
                Ok(Transaction::Dispute {
                    client: ClientId(1),
                    tx: TxId(2)
                }),
                Ok(Transaction::Resolve {
                    client: ClientId(1),
                    tx: TxId(2)
                }),
            ]
        );
    }

    #[test]
    fn amounts_on_dispute_rows_are_ignored_even_if_invalid() {
        let expected = Ok(Transaction::Chargeback {
            client: ClientId(1),
            tx: TxId(2),
        });
        assert_eq!(parse_one("chargeback,1,2,5.0"), expected);
        assert_eq!(parse_one("chargeback,1,2,not-a-number"), expected);
    }

    #[test]
    fn columns_are_matched_by_header_name() {
        let rows = parse_rows("client,amount,type,tx\n4,1.25,deposit,8\n");
        assert_eq!(
            rows,
            vec![Ok(Transaction::Deposit {
                client: ClientId(4),
                tx: TxId(8),
                amount: amt("1.25"),
            })]
        );
    }

    #[test]
    fn deposit_and_withdrawal_require_an_amount() {
        assert_eq!(parse_one("deposit,1,1,"), Err(ParseError::MissingAmount));
        assert_eq!(
            parse_rows("type,client,tx,amount\nwithdrawal,1,1\n"),
            vec![Err(ParseError::MissingAmount)]
        );
    }

    #[test]
    fn deposit_and_withdrawal_amounts_must_be_positive() {
        assert_eq!(
            parse_one("deposit,1,1,0"),
            Err(ParseError::NonPositiveAmount)
        );
        assert_eq!(
            parse_one("deposit,1,1,0.0000"),
            Err(ParseError::NonPositiveAmount)
        );
        assert_eq!(
            parse_one("withdrawal,1,1,-2"),
            Err(ParseError::NonPositiveAmount)
        );
    }

    #[test]
    fn invalid_amounts_are_reported() {
        assert_eq!(
            parse_one("deposit,1,1,1.23456"),
            Err(ParseError::InvalidAmount(ParseAmountError::TooPrecise))
        );
        assert_eq!(
            parse_one("deposit,1,1,abc"),
            Err(ParseError::InvalidAmount(ParseAmountError::Invalid))
        );
    }

    #[test]
    fn unknown_types_are_rejected() {
        assert_eq!(
            parse_one("transfer,1,1,1.0"),
            Err(ParseError::UnknownType("transfer".into()))
        );
        assert_eq!(
            parse_one("Deposit,1,1,1.0"),
            Err(ParseError::UnknownType("Deposit".into()))
        );
    }

    #[test]
    fn ids_out_of_range_or_not_numeric_are_malformed() {
        for row in [
            "deposit,70000,1,1.0",      // client is a u16
            "deposit,-1,1,1.0",         // ids are unsigned
            "deposit,1,4294967296,1.0", // tx is a u32
            "deposit,one,1,1.0",
            "deposit,,1,1.0",
        ] {
            assert!(
                matches!(parse_one(row), Err(ParseError::Malformed(_))),
                "row {row:?}"
            );
        }
    }

    #[test]
    fn client_and_tx_accessors_cover_every_variant() {
        for row in [
            "deposit,5,6,1",
            "withdrawal,5,6,1",
            "dispute,5,6,",
            "resolve,5,6,",
            "chargeback,5,6,",
        ] {
            let transaction = parse_one(row).unwrap();
            assert_eq!(transaction.client(), ClientId(5));
            assert_eq!(transaction.tx(), TxId(6));
        }
    }
}
