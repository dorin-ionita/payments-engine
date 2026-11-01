//! Writes a large, deterministic transaction CSV to stdout, for performance
//! testing:
//!
//! ```text
//! cargo run --release --example generate -- <rows> [seed] > big.csv
//! ```
//!
//! The mix is roughly 55% deposits, 30% withdrawals, 8% disputes, 6%
//! resolves, 0.02% chargebacks and 1% malformed rows, spread over every
//! possible client. Chargebacks are rare on purpose: each one locks an
//! account, and with many of them most later rows would just be refused.
//! Each transaction id is tied to a fixed client, so disputes always name
//! the right owner and exercise the dispute path rather than being refused
//! as mismatches.

use std::io::{self, BufWriter, Write};

use payments_engine::Amount;

const CLIENTS: u64 = u16::MAX as u64;
/// How many recent disputes resolves and chargebacks can pick from.
const RECENT_DISPUTES: usize = 1024;

/// A small, fast pseudo-random generator (xorshift64*). Not for security;
/// only to make the output varied but reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// The client that owns transaction `tx`: a fixed, well-mixed mapping.
fn client_of(tx: u64) -> u64 {
    tx.wrapping_mul(0x9E37_79B9_7F4A_7C15) % CLIENTS + 1
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let rows: u64 = match args.next().and_then(|arg| arg.parse().ok()) {
        Some(rows) => rows,
        None => {
            eprintln!("usage: generate <rows> [seed]");
            std::process::exit(2);
        }
    };
    let seed = args.next().and_then(|arg| arg.parse().ok()).unwrap_or(42);

    let mut rng = Rng(seed | 1); // xorshift must not start at zero
    let mut out = BufWriter::new(io::stdout().lock());
    let mut next_tx: u64 = 1;
    let mut disputed: Vec<u64> = Vec::with_capacity(RECENT_DISPUTES);

    writeln!(out, "type,client,tx,amount")?;
    for _ in 0..rows {
        // Ids are only consumed by deposits and withdrawals; disputes refer
        // back to one of them (when that is a withdrawal it is refused as
        // unsupported, which is realistic noise).
        let roll = rng.below(10_000);
        if roll < 8_500 || next_tx == 1 {
            let tx = next_tx;
            next_tx += 1;
            // Deposits up to 100, withdrawals up to 20, so most withdrawals
            // succeed and the ledger actually grows.
            let (kind, max_units) = if roll < 5_500 || tx == 1 {
                ("deposit", 1_000_000)
            } else {
                ("withdrawal", 200_000)
            };
            let amount = Amount::from_units(rng.below(max_units) as i64 + 1);
            writeln!(out, "{kind},{},{tx},{amount}", client_of(tx))?;
        } else if roll < 9_300 {
            let tx = rng.below(next_tx - 1) + 1;
            writeln!(out, "dispute,{},{tx},", client_of(tx))?;
            if disputed.len() == RECENT_DISPUTES {
                disputed.swap_remove(rng.below(RECENT_DISPUTES as u64) as usize);
            }
            disputed.push(tx);
        } else if roll < 9_902 && !disputed.is_empty() {
            let tx = disputed.swap_remove(rng.below(disputed.len() as u64) as usize);
            let kind = if roll < 9_900 {
                "resolve"
            } else {
                "chargeback"
            };
            writeln!(out, "{kind},{},{tx},", client_of(tx))?;
        } else {
            writeln!(out, "deposit,{},not-a-tx,1.0", rng.below(CLIENTS) + 1)?;
        }
    }
    out.flush()
}
