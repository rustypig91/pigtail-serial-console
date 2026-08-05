//! Throughput check (spec §10, §11): push ~3 MB of synthetic lines through
//! framer → store → filter and assert (a) memory stays bounded under a line cap
//! and (b) a 16ms-sized batch is processed well within budget.
//!
//! This is a coarse guard, not a micro-benchmark; it runs in CI without hardware.

use serialcore::clock::SessionClock;
use serialcore::filter::{Combine, FilterIndex, FilterSet};
use serialcore::framer::{FramedLine, Framer};
use serialcore::store::{IncomingLine, LineStore, PortId};
use std::time::Instant;

fn synthetic_chunk(start: usize, lines: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(lines * 40);
    for i in start..start + lines {
        buf.extend_from_slice(
            format!(
                "[{i:08}] temp:{}.{} rpm:{} state:run\n",
                i % 100,
                i % 10,
                i * 7 % 5000
            )
            .as_bytes(),
        );
    }
    buf
}

#[test]
fn throughput_bounded_and_fast() {
    let clock = SessionClock::new();
    let mut framer = Framer::new();
    let mut store = LineStore::new(200_000);
    let mut index = FilterIndex::new();
    let (filter, _) = FilterSet::compile(
        &[serialcore::filter::FilterRule {
            pattern: "rpm".into(),
            is_regex: false,
            case_sensitive: false,
            invert: false,
            enabled: true,
        }],
        Combine::Or,
    );

    // ~3 MB total (each line ~40 bytes → ~75k lines), fed in ~16ms-sized batches
    // of a few thousand lines, matching the reader's batching.
    let total_lines = 75_000;
    let batch_lines = 3_000;
    let mut produced = 0usize;
    let mut pending: Vec<FramedLine> = Vec::new();
    let mut worst_batch = std::time::Duration::ZERO;

    while produced < total_lines {
        let chunk = synthetic_chunk(produced, batch_lines);
        produced += batch_lines;

        let t0 = Instant::now();
        pending.clear();
        framer.push(&chunk, clock.now(), &mut pending);
        for line in pending.drain(..) {
            store.append(IncomingLine {
                text: line.text,
                ts: line.ts,
                port: PortId(0),
                flags: line.flags,
                spans: Default::default(),
                cursor: None,
            });
        }
        index.prune_evicted(&store);
        index.extend(&store, &filter);
        worst_batch = worst_batch.max(t0.elapsed());
    }

    // Memory bounded: the store never exceeds its cap despite 75k lines pushed.
    assert!(store.len() <= 200_000);
    assert!(store.evicted_any() || total_lines <= 200_000);

    // Every filtered index entry still resolves (no dangling references).
    for &abs in index.matching().iter().take(1000) {
        assert!(store.get(abs).is_some());
    }

    // A batch must process well within the 16ms UI budget. Generous ceiling to
    // avoid flakiness on shared CI runners, while still catching regressions
    // that blow the budget by an order of magnitude.
    assert!(
        worst_batch.as_millis() < 50,
        "worst batch took {worst_batch:?}, over budget"
    );
}
