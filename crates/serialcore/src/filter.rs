//! Filtering: which lines are *displayed*. Distinct from search (which navigates
//! without hiding). See spec §7.8.
//!
//! The filter applies to history, not just future output: changing a filter
//! reveals matching lines that already scrolled past. The index is a
//! `Vec<u64>` of matching *absolute* line indices; when new lines arrive and the
//! filter is unchanged, only the new lines are tested and the index is extended
//! (the hot path). When the filter changes, the whole index is rebuilt.

use crate::store::LineStore;
use regex::{Regex, RegexBuilder};

/// How to combine multiple filter rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Combine {
    And,
    Or,
}

/// A single user filter rule, as configured in the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterRule {
    pub pattern: String,
    /// Regex if true, plain substring if false (spec offers both, §7.8).
    pub is_regex: bool,
    pub case_sensitive: bool,
    pub invert: bool,
    pub enabled: bool,
}

impl Default for FilterRule {
    fn default() -> Self {
        FilterRule {
            pattern: String::new(),
            is_regex: true,
            case_sensitive: false,
            invert: false,
            enabled: true,
        }
    }
}

struct CompiledRule {
    re: Regex,
    invert: bool,
}

impl CompiledRule {
    fn is_match(&self, text: &str) -> bool {
        self.re.is_match(text) ^ self.invert
    }
}

/// A compiled set of filter rules.
pub struct FilterSet {
    rules: Vec<CompiledRule>,
    combine: Combine,
}

impl FilterSet {
    /// Compile enabled rules. Rules with empty patterns are ignored; rules that
    /// fail to compile are reported (by index) and excluded.
    pub fn compile(rules: &[FilterRule], combine: Combine) -> (FilterSet, Vec<(usize, String)>) {
        let mut compiled = Vec::new();
        let mut errors = Vec::new();
        for (i, rule) in rules.iter().enumerate() {
            if !rule.enabled || rule.pattern.is_empty() {
                continue;
            }
            let pattern = if rule.is_regex {
                rule.pattern.clone()
            } else {
                regex::escape(&rule.pattern)
            };
            match RegexBuilder::new(&pattern)
                .case_insensitive(!rule.case_sensitive)
                .build()
            {
                Ok(re) => compiled.push(CompiledRule {
                    re,
                    invert: rule.invert,
                }),
                Err(e) => errors.push((i, e.to_string())),
            }
        }
        (
            FilterSet {
                rules: compiled,
                combine,
            },
            errors,
        )
    }

    /// True if any rule is active (otherwise every line passes).
    pub fn is_active(&self) -> bool {
        !self.rules.is_empty()
    }

    /// Does `text` pass the filter? With no active rules, everything passes.
    pub fn matches(&self, text: &str) -> bool {
        if self.rules.is_empty() {
            return true;
        }
        match self.combine {
            Combine::And => self.rules.iter().all(|r| r.is_match(text)),
            Combine::Or => self.rules.iter().any(|r| r.is_match(text)),
        }
    }
}

/// Incrementally-maintained index of lines passing the current filter.
#[derive(Default)]
pub struct FilterIndex {
    /// Matching absolute line indices, ascending.
    matching: Vec<u64>,
    /// Next absolute index not yet tested.
    next_to_test: u64,
}

impl FilterIndex {
    pub fn new() -> FilterIndex {
        FilterIndex::default()
    }

    /// The matching absolute indices.
    pub fn matching(&self) -> &[u64] {
        &self.matching
    }

    pub fn len(&self) -> usize {
        self.matching.len()
    }

    pub fn is_empty(&self) -> bool {
        self.matching.is_empty()
    }

    /// Test only new lines and extend the index (the hot path). Safe to call
    /// every frame.
    pub fn extend(&mut self, store: &LineStore, set: &FilterSet) {
        let end = store.next_abs_index();
        let start = self.next_to_test.max(store.first_abs_index());
        for abs in start..end {
            if let Some(line) = store.get(abs) {
                if set.matches(line.text) {
                    self.matching.push(abs);
                }
            }
        }
        self.next_to_test = end;
    }

    /// Drop indices that have been evicted from the store front. Cheap: the
    /// matching vec is sorted, so this is a prefix trim.
    pub fn prune_evicted(&mut self, store: &LineStore) {
        let first = store.first_abs_index();
        if let Some(pos) = self.matching.iter().position(|&i| i >= first) {
            if pos > 0 {
                self.matching.drain(..pos);
            }
        } else if !self.matching.is_empty() && self.matching[self.matching.len() - 1] < first {
            self.matching.clear();
        }
    }

    /// Rebuild the whole index (call when the filter changes).
    pub fn rebuild(&mut self, store: &LineStore, set: &FilterSet) {
        self.matching.clear();
        self.next_to_test = store.first_abs_index();
        self.extend(store, set);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SessionClock;
    use crate::store::{IncomingLine, LineFlags, PortId};

    fn store_with(lines: &[&str]) -> LineStore {
        let clock = SessionClock::new();
        let mut s = LineStore::new(10_000);
        for l in lines {
            s.append(IncomingLine {
                text: (*l).to_string(),
                ts: clock.now(),
                port: PortId(0),
                flags: LineFlags::default(),
                spans: Default::default(),
                cursor: None,
            });
        }
        s
    }

    fn rule(pattern: &str) -> FilterRule {
        FilterRule {
            pattern: pattern.into(),
            ..Default::default()
        }
    }

    #[test]
    fn substring_and_regex_modes() {
        let store = store_with(&["INFO ok", "ERROR bad", "WARN meh"]);
        let (set, errs) = FilterSet::compile(&[rule("ERROR")], Combine::Or);
        assert!(errs.is_empty());
        let mut idx = FilterIndex::new();
        idx.rebuild(&store, &set);
        assert_eq!(idx.matching(), &[1]);
    }

    #[test]
    fn case_insensitive_default() {
        let store = store_with(&["Error one", "clean"]);
        let (set, _) = FilterSet::compile(&[rule("error")], Combine::Or);
        let mut idx = FilterIndex::new();
        idx.rebuild(&store, &set);
        assert_eq!(idx.matching(), &[0]);
    }

    #[test]
    fn invert_and_and_combine() {
        let store = store_with(&["a foo b", "a bar b", "foo bar"]);
        let mut r1 = rule("foo");
        r1.is_regex = false;
        let mut r2 = rule("bar");
        r2.is_regex = false;
        r2.invert = true; // NOT bar
        let (set, _) = FilterSet::compile(&[r1, r2], Combine::And);
        let mut idx = FilterIndex::new();
        idx.rebuild(&store, &set);
        // has foo AND not bar -> only line 0
        assert_eq!(idx.matching(), &[0]);
    }

    #[test]
    fn empty_filter_passes_all() {
        let store = store_with(&["a", "b", "c"]);
        let (set, _) = FilterSet::compile(&[], Combine::And);
        assert!(!set.is_active());
        let mut idx = FilterIndex::new();
        idx.rebuild(&store, &set);
        assert_eq!(idx.matching(), &[0, 1, 2]);
    }

    /// The key property (spec §10): incremental extension equals a full rebuild.
    #[test]
    fn incremental_equals_rebuild() {
        let clock = SessionClock::new();
        let mut store = LineStore::new(10_000);
        let (set, _) = FilterSet::compile(&[rule("\\d+")], Combine::Or);

        let mut incremental = FilterIndex::new();
        // Append in irregular chunks, extending after each.
        let mut n = 0;
        for chunk in [3usize, 1, 7, 0, 5, 2, 9] {
            for _ in 0..chunk {
                let text = if n % 3 == 0 {
                    format!("value {n}")
                } else {
                    "no digits here".to_string()
                };
                store.append(IncomingLine {
                    text,
                    ts: clock.now(),
                    port: PortId(0),
                    flags: LineFlags::default(),
                    spans: Default::default(),
                    cursor: None,
                });
                n += 1;
            }
            incremental.extend(&store, &set);
        }

        let mut full = FilterIndex::new();
        full.rebuild(&store, &set);
        assert_eq!(incremental.matching(), full.matching());
    }

    #[test]
    fn prune_after_eviction() {
        let clock = SessionClock::new();
        let mut store = LineStore::new(100);
        let (set, _) = FilterSet::compile(&[rule("x")], Combine::Or);
        let mut idx = FilterIndex::new();
        for i in 0..250 {
            let text = if i % 2 == 0 { "x here" } else { "none" };
            store.append(IncomingLine {
                text: text.to_string(),
                ts: clock.now(),
                port: PortId(0),
                flags: LineFlags::default(),
                spans: Default::default(),
                cursor: None,
            });
            idx.extend(&store, &set);
        }
        idx.prune_evicted(&store);
        // Every surviving matching index must still resolve in the store.
        for &abs in idx.matching() {
            assert!(store.get(abs).is_some(), "index {abs} should resolve");
        }
    }
}
