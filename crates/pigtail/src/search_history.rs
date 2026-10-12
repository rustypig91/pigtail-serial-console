//! Search recall is UI state shared by every pane for this run only.

use std::collections::HashMap;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SearchEntry {
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
}

impl SearchEntry {
    pub fn new(query: &str, regex: bool, case_sensitive: bool) -> Self {
        Self {
            query: query.into(),
            regex,
            case_sensitive,
        }
    }
}

#[derive(Default)]
pub(crate) struct SearchHistory {
    pub entries: Vec<SearchEntry>,
    // A pane can switch tabs without the reused text widget losing focus.
    pending: HashMap<egui::Id, (egui::Id, SearchEntry)>,
}

impl SearchHistory {
    pub fn remember(&mut self, entry: SearchEntry) {
        if entry.query.is_empty()
            || crate::app::search_pattern(&entry.query, entry.case_sensitive, entry.regex).is_err()
        {
            return;
        }
        self.entries.retain(|old| *old != entry);
        self.entries.insert(0, entry);
        self.entries.truncate(50);
    }

    pub fn observe(&mut self, pane: egui::Id, target: egui::Id, entry: SearchEntry) {
        if let Some((old_target, old_entry)) = self.pending.insert(pane, (target, entry)) {
            if old_target != target {
                self.remember(old_entry);
            }
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct SearchBrowse {
    draft: SearchEntry,
    entries: Vec<SearchEntry>,
    position: Option<usize>,
    last: Option<SearchEntry>,
}

impl SearchBrowse {
    pub fn recall(
        &mut self,
        history: &mut SearchHistory,
        current: &SearchEntry,
        older: bool,
    ) -> SearchEntry {
        if self.last.as_ref() != Some(current) {
            *self = Self::default();
            self.draft = current.clone();
            history.remember(current.clone());
            self.entries = history
                .entries
                .iter()
                .filter(|entry| *entry != current)
                .cloned()
                .collect();
        }
        self.position = match (self.position, older) {
            (None, true) if !self.entries.is_empty() => Some(0),
            (Some(pos), true) => Some((pos + 1).min(self.entries.len() - 1)),
            (Some(pos), false) => pos.checked_sub(1),
            _ => None,
        };
        let entry = self
            .position
            .map_or_else(|| self.draft.clone(), |pos| self.entries[pos].clone());
        self.last = Some(entry.clone());
        entry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_bounded_unique_and_validates_regex() {
        let mut history = SearchHistory::default();
        for i in 0..55 {
            history.remember(SearchEntry::new(&i.to_string(), false, false));
        }
        history.remember(SearchEntry::default());
        history.remember(SearchEntry::new("[", true, false));
        assert_eq!(history.entries.len(), 50);
        history.remember(SearchEntry::new("20", false, false));
        assert_eq!(history.entries[0].query, "20");
        assert_eq!(history.entries.len(), 50);
        history.remember(SearchEntry::new("20", true, true));
        assert!(history.entries[0].regex && history.entries[0].case_sensitive);
        assert_eq!(history.entries[1].query, "20");
    }

    #[test]
    fn recall_preserves_draft_and_stops_at_ends() {
        let mut history = SearchHistory::default();
        let old = SearchEntry::new("error", true, true);
        let newer = SearchEntry::new("ready", false, false);
        let draft = SearchEntry::new("temp", false, false);
        history.remember(old.clone());
        history.remember(newer.clone());
        let mut browse = SearchBrowse::default();
        let first = browse.recall(&mut history, &draft, true);
        assert_eq!(first, newer);
        let second = browse.recall(&mut history, &first, true);
        assert_eq!(second, old);
        assert_eq!(browse.recall(&mut history, &second, true), old);
        assert_eq!(browse.recall(&mut history, &old, false), newer);
        assert_eq!(browse.recall(&mut history, &newer, false), draft);
        assert_eq!(browse.recall(&mut history, &draft, false), draft);
    }
}
