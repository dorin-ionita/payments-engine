//! Demo: many input streams processed concurrently, as a server receiving
//! CSVs over thousands of TCP connections would.
//!
//! ```text
//! cargo run --release --example concurrent -- a.csv b.csv ... > accounts.csv
//! ```
//!
//! Each input file stands in for one connection and gets a reader thread,
//! which parses rows with the library and sends each transaction to the shard
//! that owns its client (`client % shards`). Every shard thread owns a plain
//! [`Engine`] for its clients, so the hot path needs no locks. Channels are
//! bounded: when a shard falls behind, readers wait instead of letting memory
//! grow without limit.
//!
//! Known limitations (by design, see the README):
//! - Order is preserved per input stream, but rows of the same client coming
//!   from *different* streams have no defined order between them.
//! - Duplicate transaction ids are detected only within a shard.
//! - A dispute naming the wrong client is routed to that client's shard and
//!   reported as an unknown transaction rather than a client mismatch.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::thread;

use anyhow::{anyhow, bail, Context};
use csv::StringRecord;
use payments_engine::{csv_reader, report, Engine, Transaction};

/// Transactions are sent to shards in batches: one channel operation per
/// transaction would cost more than processing the transaction itself.
const BATCH_SIZE: usize = 256;
/// Batches that may wait in each shard's queue before readers block.
const QUEUE_CAPACITY: usize = 16;

/// A transaction plus where it came from, for error messages.
struct Job {
    transaction: Transaction,
    stream: usize,
    line: u64,
}

type Batch = Vec<Job>;

/// Writes one diagnostic line to stderr. The line is formatted first so it
/// goes out in a single write and lines from different threads never
/// interleave. (`eprint!` rather than a raw stderr handle, so that test
/// runs capture the output.)
fn log(message: String) {
    let line = message + "\n";
    eprint!("{line}");
}

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
    let paths: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if paths.is_empty() {
        bail!("usage: concurrent <transactions.csv>...");
    }
    let inputs = paths
        .iter()
        .map(|path| File::open(path).with_context(|| format!("cannot open {}", path.display())))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let shards = thread::available_parallelism().map_or(4, |n| n.get());

    let engines = run_sharded(inputs, shards)?;

    // Shards own disjoint clients, so their accounts can simply be combined.
    let accounts = engines.iter().flat_map(Engine::accounts);
    report::write_accounts(accounts, BufWriter::new(io::stdout().lock()))
        .context("cannot write the output")
}

/// Processes every input concurrently and returns one engine per shard.
/// Fails if any input cannot be read, in which case nothing should be output.
fn run_sharded<R: Read + Send + 'static>(
    inputs: Vec<R>,
    shards: usize,
) -> anyhow::Result<Vec<Engine>> {
    let (queues, workers): (Vec<_>, Vec<_>) = (0..shards.max(1))
        .map(|_| {
            let (queue, jobs) = sync_channel::<Batch>(QUEUE_CAPACITY);
            (queue, thread::spawn(move || shard_worker(jobs)))
        })
        .unzip();

    let readers: Vec<_> = inputs
        .into_iter()
        .enumerate()
        .map(|(index, input)| {
            let queues = queues.clone();
            thread::spawn(move || read_stream(index + 1, input, &queues))
        })
        .collect();
    // Shards stop once every reader has finished and dropped its queues.
    drop(queues);

    let mut first_failure = None;
    for reader in readers {
        let result = reader
            .join()
            .unwrap_or_else(|_| Err(anyhow!("a reader thread panicked")));
        if let Err(err) = result {
            first_failure.get_or_insert(err);
        }
    }
    let engines = workers
        .into_iter()
        .map(|worker| {
            worker
                .join()
                .map_err(|_| anyhow!("a shard thread panicked"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    match first_failure {
        Some(err) => Err(err),
        None => Ok(engines),
    }
}

/// Reads one input stream and routes each valid transaction to its shard.
/// Batches keep the stream's order, so each client's transactions from this
/// stream reach its shard in input order.
fn read_stream<R: Read>(
    stream: usize,
    input: R,
    queues: &[SyncSender<Batch>],
) -> anyhow::Result<()> {
    let mut reader = csv_reader(BufReader::new(input));
    let headers = reader
        .headers()
        .with_context(|| format!("stream {stream}: cannot read the header row"))?
        .clone();
    let mut record = StringRecord::new();
    let mut batches: Vec<Batch> = queues.iter().map(|_| Batch::new()).collect();

    loop {
        match reader.read_record(&mut record) {
            Ok(false) => break,
            Ok(true) => {
                let line = record.position().map_or(0, |pos| pos.line());
                match Transaction::from_record(&record, &headers) {
                    Ok(transaction) => {
                        let shard = usize::from(transaction.client().0) % queues.len();
                        let batch = &mut batches[shard];
                        batch.push(Job {
                            transaction,
                            stream,
                            line,
                        });
                        if batch.len() == BATCH_SIZE {
                            send(&queues[shard], batch, stream)?;
                        }
                    }
                    Err(err) => log(format!("warning: stream {stream} line {line}: {err}")),
                }
            }
            Err(err) if err.is_io_error() => {
                return Err(err).context(format!("stream {stream}: read failed"));
            }
            Err(err) => {
                let line = err.position().map_or(0, |pos| pos.line());
                log(format!(
                    "warning: stream {stream} line {line}: unreadable row: {err}"
                ));
            }
        }
    }
    for (queue, batch) in queues.iter().zip(&mut batches) {
        if !batch.is_empty() {
            send(queue, batch, stream)?;
        }
    }
    Ok(())
}

/// Sends a full batch to its shard and leaves an empty one in its place.
fn send(queue: &SyncSender<Batch>, batch: &mut Batch, stream: usize) -> anyhow::Result<()> {
    let full = std::mem::replace(batch, Batch::with_capacity(BATCH_SIZE));
    queue
        .send(full)
        .map_err(|_| anyhow!("stream {stream}: a shard stopped early"))
}

/// Applies every job sent to this shard, in arrival order.
fn shard_worker(batches: Receiver<Batch>) -> Engine {
    let mut engine = Engine::new();
    for Job {
        transaction,
        stream,
        line,
    } in batches.into_iter().flatten()
    {
        match engine.process(transaction) {
            Ok(None) => {}
            Ok(Some(notice)) => log(format!("notice: stream {stream} line {line}: {notice}")),
            Err(err) => log(format!("warning: stream {stream} line {line}: {err}")),
        }
    }
    engine
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Cursor;

    use payments_engine::{Account, ClientId};

    use super::*;

    const HEADER: &str = "type,client,tx,amount\n";

    /// A deterministic mix of rows over 40 clients with globally unique tx
    /// ids, where disputes only name the client's own deposits. Returns
    /// `(client, row)` pairs in chronological order.
    fn generate_rows(count: usize) -> Vec<(u16, String)> {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut random = |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        let mut deposits: BTreeMap<u16, Vec<u32>> = BTreeMap::new();
        let mut rows = Vec::with_capacity(count);
        for tx in 1..=count as u32 {
            let client = random(40) as u16 + 1;
            let own = deposits.entry(client).or_default();
            let row = match random(100) {
                0..=49 => {
                    own.push(tx);
                    format!(
                        "deposit,{client},{tx},{}.{:04}",
                        random(100),
                        random(10_000)
                    )
                }
                50..=74 => format!("withdrawal,{client},{tx},{}", random(30) + 1),
                75..=98 if !own.is_empty() => {
                    let target = own[random(own.len() as u64) as usize];
                    let kind = ["dispute", "dispute", "resolve"][random(3) as usize];
                    format!("{kind},{client},{target},")
                }
                _ if !own.is_empty() => {
                    let target = own[random(own.len() as u64) as usize];
                    format!("chargeback,{client},{target},")
                }
                _ => format!("deposit,{client},{tx},1"),
            };
            rows.push((client, row));
        }
        rows
    }

    fn accounts_of<'a>(
        engines: impl IntoIterator<Item = &'a Engine>,
    ) -> BTreeMap<ClientId, Account> {
        engines
            .into_iter()
            .flat_map(Engine::accounts)
            .map(|(client, account)| (client, account.clone()))
            .collect()
    }

    /// The same rows processed by one sequential engine, as the CLI does.
    fn sequential(rows: &[(u16, String)]) -> Engine {
        let input: String = std::iter::once(HEADER.to_owned())
            .chain(rows.iter().map(|(_, row)| format!("{row}\n")))
            .collect();
        let mut reader = csv_reader(input.as_bytes());
        let headers = reader.headers().unwrap().clone();
        let mut engine = Engine::new();
        for record in reader.records() {
            if let Ok(transaction) = Transaction::from_record(&record.unwrap(), &headers) {
                let _ = engine.process(transaction);
            }
        }
        engine
    }

    /// Splits rows by client into `count` streams, keeping each client's
    /// rows in their original order within a single stream.
    fn split_by_client(rows: &[(u16, String)], count: usize) -> Vec<Cursor<Vec<u8>>> {
        let mut streams = vec![HEADER.to_owned(); count];
        for (client, row) in rows {
            let stream = &mut streams[usize::from(*client) % count];
            stream.push_str(row);
            stream.push('\n');
        }
        streams
            .into_iter()
            .map(|s| Cursor::new(s.into_bytes()))
            .collect()
    }

    #[test]
    fn sharded_result_matches_sequential_result() {
        let rows = generate_rows(20_000);
        let expected = accounts_of([&sequential(&rows)]);
        assert!(
            expected.values().any(Account::is_locked),
            "test data has no chargebacks"
        );

        for (streams, shards) in [(1, 1), (3, 4), (7, 2), (16, 8)] {
            let engines = run_sharded(split_by_client(&rows, streams), shards).unwrap();
            assert_eq!(
                accounts_of(&engines),
                expected,
                "{streams} streams, {shards} shards"
            );
        }
    }

    #[test]
    fn a_failing_stream_fails_the_whole_run() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("disk on fire"))
            }
        }
        let result = run_sharded(vec![Box::new(Broken) as Box<dyn Read + Send>], 2);
        assert!(result.is_err());
    }
}
