//! Property test: framing an input in one shot equals framing it split at any
//! set of boundaries (spec §10). This is where the real bugs live.

use proptest::prelude::*;
use serialcore::clock::SessionClock;
use serialcore::framer::Framer;

fn frame_in_chunks(input: &[u8], splits: &[usize]) -> Vec<String> {
    let clock = SessionClock::new();
    let mut f = Framer::new();
    let mut out = Vec::new();
    let mut prev = 0usize;
    let mut bounds: Vec<usize> = splits
        .iter()
        .copied()
        .filter(|&s| s <= input.len())
        .collect();
    bounds.sort_unstable();
    bounds.dedup();
    for &b in &bounds {
        f.push(&input[prev..b], clock.now(), &mut out);
        prev = b;
    }
    f.push(&input[prev..], clock.now(), &mut out);
    f.flush_final(&mut out);
    out.into_iter().map(|l| l.text).collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// Arbitrary bytes (favouring terminators and UTF-8 edge bytes), split at
    /// arbitrary boundaries, must equal the whole-input framing.
    #[test]
    fn split_invariance(
        input in proptest::collection::vec(
            prop_oneof![
                Just(b'\n'),
                Just(b'\r'),
                Just(b'a'),
                Just(0xFFu8),
                Just(0xC3u8),
                any::<u8>(),
            ],
            0..300,
        ),
        splits in proptest::collection::vec(0usize..300, 0..8),
    ) {
        let reference = frame_in_chunks(&input, &[]);
        let split = frame_in_chunks(&input, &splits);
        prop_assert_eq!(split, reference);
    }
}
