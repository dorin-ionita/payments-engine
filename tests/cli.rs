//! End-to-end tests: run the real binary on the hand-written scenarios in
//! `tests/fixtures/` and compare its output with `<name>.expected.csv`.
//!
//! Outputs are compared as parsed values, so row order and number formatting
//! (`1.5` vs `1.5000`) cannot cause false failures, while any difference in a
//! balance or lock flag does. Each test also checks exactly which input lines
//! were reported on stderr.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use payments_engine::Amount;

/// client -> (available, held, total, locked)
type Accounts = BTreeMap<u16, (Amount, Amount, Amount, bool)>;

fn run_cli(args: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_payments_engine"))
        .args(args)
        .output()
        .expect("failed to run the binary")
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Parses an accounts CSV strictly: exact header, five columns, valid values.
fn parse_accounts(csv: &str) -> Accounts {
    let mut lines = csv.lines();
    assert_eq!(
        lines.next().map(str::trim),
        Some("client,available,held,total,locked"),
        "unexpected header in:\n{csv}"
    );
    lines
        .map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            let [client, available, held, total, locked] = fields[..] else {
                panic!("expected 5 columns in {line:?}");
            };
            let amount = |s: &str| s.parse::<Amount>().unwrap_or_else(|e| panic!("{s:?}: {e}"));
            let locked = match locked {
                "true" => true,
                "false" => false,
                other => panic!("invalid locked flag {other:?}"),
            };
            let client: u16 = client.parse().expect("invalid client id");
            let row = (amount(available), amount(held), amount(total), locked);
            (client, row)
        })
        .collect()
}

/// Input line numbers that stderr reports with the given prefix
/// (`"warning"` or `"notice"`).
fn reported_lines(stderr: &str, prefix: &str) -> Vec<u64> {
    let marker = format!("{prefix}: line ");
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix(&marker))
        .map(|rest| rest.split(':').next().unwrap().parse().unwrap())
        .collect()
}

/// Runs fixture `name` and checks the balances and the reported lines.
fn check_fixture(name: &str, warned_lines: &[u64], noticed_lines: &[u64]) {
    let output = run_cli(&[&fixture(&format!("{name}.csv"))]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        output.status.success(),
        "exit status {}; stderr:\n{stderr}",
        output.status
    );

    let expected = std::fs::read_to_string(fixture(&format!("{name}.expected.csv"))).unwrap();
    assert_eq!(
        parse_accounts(&stdout),
        parse_accounts(&expected),
        "stderr:\n{stderr}"
    );
    assert_eq!(
        reported_lines(&stderr, "warning"),
        warned_lines,
        "stderr:\n{stderr}"
    );
    assert_eq!(
        reported_lines(&stderr, "notice"),
        noticed_lines,
        "stderr:\n{stderr}"
    );
}

// --- scenarios ----------------------------------------------------------------

/// Clients and tx ids out of order. Client 12: 50.25 - 20.25 = 30, and a
/// withdrawal of 30.0001 is refused (line 7). Client 3: 10 + 0.5 - 10.5 = 0
/// (exact-balance withdrawal). Client 40 nets to 0. Client 55 only has a
/// refused withdrawal (line 10) but still gets a row (assumption 7).
#[test]
fn deposits_withdrawals() {
    check_fixture("deposits_withdrawals", &[7, 10], &[]);
}

/// Padded headers and fields, a tab, `.5`, `5.`, `1.50000`, rows with and
/// without the trailing amount column. Deposits .5 + 5 + 1.5 + 0.0001 =
/// 7.0001, minus 2.0001 = 5. Disputing tx 4 holds 0.0001; tx 1 is disputed
/// and resolved, so only 0.0001 stays held: 4.9999 / 0.0001 / 5.
#[test]
fn whitespace_and_precision() {
    check_fixture("whitespace_and_precision", &[], &[]);
}

/// 100 + 40.5 deposited; disputing tx 2 holds 40.5, so withdrawing 120 is
/// refused even though the total is 140.5 (line 5). Withdraw 70 -> 30
/// available; resolve -> 70.5; withdraw 70 -> 0.5.
#[test]
fn dispute_resolve() {
    check_fixture("dispute_resolve", &[5], &[]);
}

/// Client 30: 25 + 5.75; the 25 deposit is disputed and charged back,
/// leaving 5.75 and a locked account. The later deposit, withdrawal and
/// dispute on client 30 are refused (lines 7-9). Client 31 is unaffected.
#[test]
fn dispute_chargeback() {
    check_fixture("dispute_chargeback", &[7, 8, 9], &[]);
}

/// Deposit 1000, withdraw 950, then the deposit is disputed: available goes
/// to -950 with 1000 held (notice on line 4). The chargeback removes the
/// 1000 and locks the account: -950 / 0 / -950. A later withdrawal is
/// refused (line 6).
#[test]
fn withdrawn_then_disputed() {
    check_fixture("withdrawn_then_disputed", &[6], &[4]);
}

/// Both deposits (8 and 2.5) are disputed; tx 1 is charged back and locks
/// the account. tx 2 can then be neither resolved nor charged back (lines
/// 7-8), so its 2.5 stays held until a human acts.
#[test]
fn locked_with_pending_dispute() {
    check_fixture("locked_with_pending_dispute", &[7, 8], &[]);
}

/// Dispute, resolve and chargeback of a withdrawal are all refused
/// (lines 4-6); the balance stays 12 - 4 = 8.
#[test]
fn withdrawal_dispute_unsupported() {
    check_fixture("withdrawal_dispute_unsupported", &[4, 5, 6], &[]);
}

/// Unknown tx (4), wrong client (5), resolve and chargeback without a
/// dispute (6, 7), a second dispute while open (9), a dispute after the
/// resolve (11), and duplicate tx ids for a deposit (12) and a withdrawal
/// (13). The withdrawal still creates client 92 with a zero balance.
#[test]
fn invalid_references() {
    check_fixture("invalid_references", &[4, 5, 6, 7, 9, 11, 12, 13], &[]);
}

/// Every row between the two valid deposits (10 and 2.5) is malformed:
/// unknown type, missing / negative / too precise / non-numeric / zero
/// amounts, an out-of-range client, a missing amount column, a non-numeric
/// client. Each is reported and skipped.
#[test]
fn malformed_rows() {
    check_fixture("malformed_rows", &[3, 4, 5, 6, 7, 8, 9, 10, 11], &[]);
}

// --- CLI contract ---------------------------------------------------------------

#[test]
fn missing_argument_fails_without_output() {
    let output = run_cli(&[]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage"));
}

#[test]
fn extra_argument_fails_without_output() {
    let path = fixture("deposits_withdrawals.csv");
    let output = run_cli(&[&path, &path]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn missing_file_fails_without_output() {
    let output = run_cli(&[&fixture("does_not_exist.csv")]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot open"));
}

#[test]
fn header_only_input_prints_header_only() {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("header_only.csv");
    std::fs::write(&path, "type,client,tx,amount\n").unwrap();
    let output = run_cli(&[&path]);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"client,available,held,total,locked\n");
}

#[test]
fn empty_input_prints_header_only() {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("empty.csv");
    std::fs::write(&path, "").unwrap();
    let output = run_cli(&[&path]);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"client,available,held,total,locked\n");
}

#[test]
fn stdout_carries_only_the_csv_even_when_rows_fail() {
    let output = run_cli(&[&fixture("malformed_rows.csv")]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    // parse_accounts panics on anything that is not a well-formed row.
    assert_eq!(parse_accounts(&stdout).len(), 1);
    assert!(!stdout.contains("warning") && !stdout.contains("line"));
}
