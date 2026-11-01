# payments_engine

A small payments engine. It reads a CSV of client transactions (deposits,
withdrawals, disputes, resolutions and chargebacks), applies them in order to
each client's account, and prints the final state of every account as CSV.

```sh
cargo run -- transactions.csv > accounts.csv
```

| | |
|---|---|
| **Input** | CSV with the columns `type, client, tx, amount`. `client` is a `u16`, `tx` a `u32`, `amount` a decimal with up to four places. Whitespace around fields is fine, and dispute-style rows may leave the amount empty or omit the column. |
| **Output (stdout)** | `client,available,held,total,locked`, one row per client, sorted by client id, amounts always with four decimals. |
| **Diagnostics (stderr)** | One line per refused or malformed row (`warning: line N: …`) and per notable event (`notice: line N: …`). Stdout never carries anything but the CSV. |
| **Exit code** | `0` when the input was processed, even if some rows were refused. `1` when the input can't be read or the output can't be written; in that case **nothing** is printed to stdout. |

## Contents

- [Architecture](#architecture)
- [Assumptions and decisions](#assumptions-and-decisions)
- [Correctness](#correctness)
- [Safety and error handling](#safety-and-error-handling)
- [Performance and scaling](#performance-and-scaling)
- [Known limitations](#known-limitations)
- [AI assistance](#ai-assistance)

## Architecture

```text
             src/transaction.rs           src/engine.rs                 src/report.rs
CSV rows ──► parse & validate ──────────► Engine::process ─────────────► write_accounts ──► CSV
             (Transaction or ParseError)  │ accounts: client → Account   (sorted, 4 dp)
                                          │ ledger:   tx → StoredTx
                                          ▼
                                          src/account.rs (balance operations)
```

| Module | Responsibility |
|---|---|
| `amount.rs` | `Amount`: exact fixed-point money (an `i64` count of 1/10 000 units). |
| `transaction.rs` | The parsing boundary: a CSV row becomes a valid `Transaction` or a `ParseError`. |
| `account.rs` | One client's balances. Moves money between *available* and *held*; refuses everything once locked. |
| `engine.rs` | Applies transactions, keeps the ledger of past deposits and withdrawals, runs the dispute lifecycle. |
| `error.rs` | `EngineError`: every reason a transaction can be refused, i.e. the catalogue of business rules. |
| `report.rs` | Writes the accounts as CSV. |
| `main.rs` | The CLI: argument, file reading, stderr reporting, exit codes. |

The library does no I/O of its own: it never prints, exits or opens files.
`main.rs` is the only place that decides what is fatal. That keeps the engine
usable from other front ends, such as the [concurrent demo](#many-concurrent-streams).

A deposit goes through a small dispute lifecycle, implemented as one
exhaustive `match` in `Engine::settle`:

```text
Normal ──dispute──► Disputed ──resolve────► Resolved     (final)
                             └─chargeback─► ChargedBack  (final; account locked)
```

## Assumptions and decisions

Where the requirements leave room for interpretation, the engine behaves the
way a cautious bank would, and every such choice is listed here. In the code,
each rule cites its number (`// Assumption 4: …`).

1. **Only deposits can be disputed.** Disputing a withdrawal would mean
   holding money the client already took out. Withdrawals are still recorded,
   so a dispute, resolve or chargeback that names one is refused with a
   specific message (*disputes of withdrawals are not supported*) rather than
   a generic "unknown transaction". Real withdrawal disputes are a separate
   process at a bank.
2. **Clients can only act on their own transactions.** A dispute, resolve or
   chargeback whose client differs from the original transaction's client is
   refused.
3. **A locked account accepts nothing.** After a chargeback, every later
   deposit, withdrawal, dispute, resolve and chargeback on that account is
   refused. This includes disputes that were already open when the account
   was locked: their funds stay *held* until a person settles them outside the
   engine.
4. **A dispute is accepted even if the funds were already withdrawn.** The
   full amount moves to *held* and *available* goes negative: the client now
   owes that money. The reasoning:
   - When a partner reverses a deposit, the money is gone whether or not the
     engine records it. Ignoring the reversal would hide the loss, not avoid it.
   - A negative balance is how a bank records what a customer owes (much
     like a bounced cheque producing an overdraft); recovering it is then a
     matter for people, not the engine.
   - It is the only reading in which a deposit → withdraw → reverse fraud
     pattern ends with the account locked.

   A `notice:` line on stderr flags every dispute that leaves a balance
   negative. Who ultimately carries such a loss is a business decision, so the
   engine also offers `DisputePolicy::RejectIfInsufficient`, which refuses such
   disputes instead. The CLI always uses the default, `AllowNegative`.
5. **A transaction can be disputed only once.** *Resolved* and *ChargedBack*
   are final states. A second dispute of the same transaction is refused and
   reported as needing manual review; repeated disputes are handled by people.
   This also stops anyone from freezing the same funds over and over.
6. **Transaction ids are unique.** A deposit or withdrawal that reuses an id
   already accepted is refused; the first one wins. Ids of *refused*
   transactions are not remembered, so they can't be disputed later.
7. **Any deposit or withdrawal row creates its client**, even if the
   transaction itself is refused (the client then shows zero balances).
   Dispute, resolve and chargeback rows never create clients, and neither do
   rows that fail to parse.
8. **Malformed rows are skipped.** An unknown type, a bad id, or a missing,
   zero, negative or unparsable amount produces a `warning:` with the line
   number, and processing continues.
9. **Precision.** Amounts are exact; nothing is ever rounded.
   - Up to four decimals are kept. More are accepted only when the extra
     digits are zeros (`1.50000`). Anything that would need rounding
     (`1.23456`) is refused rather than silently changed.
   - Shorthand forms `.5` and `5.` are accepted.
   - Output always shows four decimals (`1.5000`).
10. **A failure in the middle of the input is fatal.** If the file can't be
    read to the end, the program prints an error and exits with `1` without
    printing any balances: partial balances would look exactly like final ones.
11. **Output is sorted by client id.** Any order would be valid; a stable order
    makes the output reproducible and easy to compare.

Transaction types are matched exactly (`deposit`, not `Deposit`); guessing at
variants seemed the wrong default for financial input. Columns are matched by
header name, so their order doesn't matter.

## Correctness

Correctness rests on three layers.

**1. Types that rule out whole classes of mistakes**

- Money is never a float. `Amount` is an integer number of 1/10 000 units, and
  only `checked_add` / `checked_sub` exist, so an overflow is an error rather
  than a wrong number.
- Parsing happens once, at the boundary. Dispute, resolve and chargeback
  variants of `Transaction` have **no amount field**, so an amount written on
  such a row can't be used by mistake. Deposit and withdrawal amounts are
  guaranteed positive past that point.
- `ClientId` and `TxId` are distinct types, so the two ids can't be swapped.
- An account's three balances are written in exactly one function, which
  computes `total` itself, so `total = available + held` can't drift. Every
  operation either applies completely or changes nothing.
- The dispute lifecycle is one exhaustive `match`; a new state would not
  compile until every transition handles it.

**2. Example tests** (`cargo test`)

- **64 unit tests** next to the code, covering every parsing rule and every
  business rule above, each refusal case included.
- **15 end-to-end tests** (`tests/cli.rs`) run the real binary:
  - Nine hand-written scenarios in `tests/fixtures/` (`<name>.csv` plus
    `<name>.expected.csv`). Each test's comment shows the arithmetic behind the
    expected numbers. Outputs are compared as parsed values, so formatting or
    row order can't produce false results, and each test also checks exactly
    which input lines were reported on stderr.
  - The command-line contract: missing or extra arguments and missing files
    fail with nothing on stdout; empty input prints only the header; stdout
    stays pure CSV even when many rows fail.

**3. Property tests** (`tests/properties.rs`, using `proptest`)

About a thousand random sequences of up to 120 transactions each, run under
both dispute policies.
After **every** transaction the engine is compared with a separate model kept
by the test:

- money is conserved: each client's total equals accepted deposits minus
  accepted withdrawals minus chargebacks;
- *held* always equals the sum of the client's open disputes, and is never
  negative;
- `total = available + held`;
- an account is locked exactly when one of its deposits was charged back, and
  a locked account never changes again;
- a refused transaction changes nothing;
- a withdrawal never makes *available* negative (and, under the strict policy,
  nothing does).

Further properties check that amounts print and parse back to the same value
and that arbitrary text never makes parsing panic.

To make sure the tests can actually fail, mistakes were planted on purpose
during development and each was caught: a wrong expected value (end-to-end
test), a chargeback that released funds instead of removing them (property
tests, which shrank it to a three-step example), and the concurrent demo
routing by transaction instead of by client (equivalence test).

**What this doesn't prove:** it can't show that the interpretations above
match every expectation of whoever supplies the input; it shows that the code
does exactly what this document says.

## Safety and error handling

- No `unsafe` code: `#![forbid(unsafe_code)]` in both the library and the binary.
- No panics on any input: the library denies `unwrap` and `expect` outside
  tests, input is never indexed directly, all money arithmetic is checked, and
  release builds keep integer overflow checks on as a second safety net.
- Errors come in two tiers:

  | Tier | Examples | Handling |
  |---|---|---|
  | Per row | malformed row, unknown type, any refused transaction | `warning: line N: <reason>` on stderr; the row is skipped; exit code stays `0` |
  | Fatal | wrong arguments, file missing or unreadable, read error mid-file, output can't be written (e.g. a closed pipe) | `error: <reason>` on stderr; exit code `1`; nothing on stdout |

- Library errors are typed (`ParseError`, `EngineError`, via `thiserror`);
  `anyhow` is used only in the binary.
- A failure to *write a warning* never stops processing: losing a log line is
  better than losing the run.
- Dependencies are few and widely used (`csv`, `serde`, `thiserror`, `anyhow`;
  `proptest` for tests only). CI checks them against the RustSec advisory
  database.

## Performance and scaling

**Streaming.** Input is read one record at a time into a reused buffer, and
fields are borrowed rather than copied, so the input file is never held in
memory. Each transaction takes constant time on average (hash lookups).

**Memory.** Two things are kept:

- the accounts: at most 65 536 clients, so a few MB at most;
- the ledger of accepted deposits and withdrawals. It can't be avoided, because
  a dispute may refer to any earlier transaction. It is the only thing that
  grows, and it grows with *accepted transactions*, not with file size:
  refused and malformed rows cost nothing.

Measured on an Intel i7-6700HQ (4 cores), release build, with input from
`examples/generate.rs` (seed 42, all 65 535 clients):

| Rows | Input size | Time | Peak memory |
|---|---|---|---|
| 1 M | 27 MiB | 0.97 s | 45 MB |
| 10 M | 280 MiB | 9.9 s (~1 M rows/s) | 622 MB |

The 10 M run stores about 8.4 M transactions, i.e. roughly **74 bytes per
stored transaction at peak**. A ledger entry is 25 bytes, but a hash map keeps
spare capacity and briefly holds both the old and the new table while growing.
Extrapolated to the full `u32` id space (≈ 4.3 billion transactions), that
would be a few hundred GB, so a real system needs a different strategy (see
[Known limitations](#known-limitations)).

The hash map keeps Rust's default SipHash hasher. A faster non-keyed hasher
would let anyone who controls transaction ids craft collisions and slow the
engine down to a crawl; ids come from external parties, so that resistance is
worth its small cost.

To reproduce:

```sh
cargo run --release --example generate -- 10000000 > /tmp/big.csv
/usr/bin/time -v cargo run --release -- /tmp/big.csv > /dev/null
```

### Many concurrent streams

`examples/concurrent.rs` shows how the same engine handles many input streams
at once, as a server receiving CSVs over many network connections would:

```sh
cargo run --release --example concurrent -- a.csv b.csv c.csv > accounts.csv
```

- One reader thread per stream parses rows (with the same library code as the
  CLI) and sends each transaction to the shard that owns its client
  (`client % shards`).
- Each shard thread owns a plain `Engine` for its clients, so the hot path
  needs no locks; at the end the shards' accounts are combined.
- Queues between them are bounded, so a slow shard makes readers wait instead
  of letting memory grow (backpressure).
- Transactions travel in batches of 256. A first version sent them one at a
  time and was twice as *slow* as the plain CLI: the channel overhead per
  transaction exceeded the work per transaction.

On the same 10 M rows:

| Run | Time | Peak memory |
|---|---|---|
| CLI (sequential) | 9.9 s | 622 MB |
| Demo, 1 input file | 9.9 s | 537 MB |
| Demo, 8 input files split by client | 4.3 s | 469 MB |

All three produce byte-identical output. With one input the single reader
thread is the bottleneck; the gain comes from many streams, which is the
server case. A test (`cargo test` runs it) checks that the sharded result
equals the sequential one.

The design relies on rule 2: because a client can only dispute its own
transactions, a dispute only ever needs its own client's shard. Its limits are
listed below.

## Known limitations

- **Ledger memory for huge inputs.** Every accepted transaction is remembered
  forever. A real system would bound this, for example by allowing disputes
  only within a time window and discarding older entries, or by keeping the
  ledger in an on-disk store. A more compact in-memory layout could roughly
  halve memory, at the cost of code that is harder to review.
- **Concurrent demo:** rows of the same client arriving through *different*
  streams have no defined order between them; duplicate transaction ids are
  only detected within a shard; a dispute naming the wrong client reaches that
  client's shard and is reported as an unknown transaction rather than a
  client mismatch. A real deployment would route each client's traffic
  through one ordered stream and keep a shared id registry.
- **Negative balances** (rule 4) are flagged on stderr but not otherwise
  acted on; recovery is out of scope.
- **One currency, one account per client**, no persistence between runs.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                     # unit, end-to-end, property and demo tests
```

CI (`.github/workflows/ci.yml`) runs formatting, lints and tests on stable,
the test suite on Rust 1.75 (the declared minimum version), and `cargo audit`
on every push.

The git history is meant to be read: each commit is one reviewed step, and its
message explains why it was done that way.

## AI assistance

This project was built with **Claude Code** (Anthropic's coding assistant,
model Claude Opus 5.5), working step by step under the author's review. The
full session transcript is provided with the submission.

**What the assistant did:** summarised the requirements; proposed the
architecture, the test strategy and an initial list of assumptions; wrote the
code, tests, fixtures and this README in small steps; ran the builds, tests and
measurements.

**What the author decided:** every entry in
[Assumptions and decisions](#assumptions-and-decisions), including:

- revising assumption 4 (from refusing disputes of withdrawn funds to
  accepting them with a negative balance) after weighing it against how a
  bank would treat the fraud case;
- keeping open disputes held on a locked account until a person acts, and
  making a resolve final so that repeated disputes require a person;
- the precision rules and the "no output on a fatal error" rule;
- keeping `DisputePolicy` configurable in the library;
- the scope of the concurrent demo, and which test data may be committed.

Each step was reviewed and approved before it was committed. Where the
assistant's first attempt was wrong, the history and this document say so:
the initial memory estimate, the first data generator (which locked nearly
every account) and the first concurrent design (slower than sequential) were
all measured, found wanting and corrected.
