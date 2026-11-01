//! Command-line entry point:
//!
//! ```text
//! payments_engine <transactions.csv> > accounts.csv
//! ```
//!
//! Stdout carries only the accounts CSV. Every diagnostic goes to stderr.
//! A bad row is reported and skipped (exit code 0); only problems with the
//! input file or with writing the output are fatal (non-zero exit, nothing on
//! stdout).

#![forbid(unsafe_code)]

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context};
use csv::StringRecord;
use payments_engine::{csv_reader, report, Engine, Transaction};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<()> {
    let path = input_path()?;
    let file = File::open(&path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut reader = csv_reader(BufReader::new(file));
    let headers = reader
        .headers()
        .context("cannot read the header row")?
        .clone();

    // Warnings can be numerous, so stderr is buffered. A failure to write a
    // warning must not stop processing, so those write errors are ignored.
    let mut log = BufWriter::new(io::stderr().lock());
    let mut engine = Engine::new();
    let mut record = StringRecord::new();

    loop {
        match reader.read_record(&mut record) {
            Ok(false) => break,
            Ok(true) => {
                let line = record.position().map_or(0, |pos| pos.line());
                let outcome = Transaction::from_record(&record, &headers)
                    .map_err(|err| err.to_string())
                    .and_then(|tx| engine.process(tx).map_err(|err| err.to_string()));
                match outcome {
                    Ok(None) => {}
                    Ok(Some(notice)) => {
                        let _ = writeln!(log, "notice: line {line}: {notice}");
                    }
                    Err(reason) => {
                        let _ = writeln!(log, "warning: line {line}: {reason}");
                    }
                }
            }
            // A failing read means the input itself is unreliable: stop
            // without printing balances that would look final but are not.
            Err(err) if err.is_io_error() => {
                return Err(err).context(format!("cannot read {}", path.display()));
            }
            // Anything else (e.g. invalid UTF-8) only affects this row.
            Err(err) => {
                let line = err.position().map_or(0, |pos| pos.line());
                let _ = writeln!(log, "warning: line {line}: unreadable row: {err}");
            }
        }
    }
    let _ = log.flush();

    // Output is written only after the whole input was processed.
    let stdout = BufWriter::new(io::stdout().lock());
    report::write_accounts(engine.accounts(), stdout).context("cannot write the output")?;
    Ok(())
}

/// The single required argument: the path of the input CSV. `args_os` is
/// used so that a non-UTF-8 path is accepted instead of causing a panic.
fn input_path() -> anyhow::Result<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    match (args.next(), args.next()) {
        (Some(path), None) => Ok(PathBuf::from(path)),
        _ => bail!("usage: payments_engine <transactions.csv>"),
    }
}
