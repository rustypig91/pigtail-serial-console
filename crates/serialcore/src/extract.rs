//! Numeric-series extraction rules (spec §7.13).
//!
//! Two modes:
//! - *Key-value*: `temp:23.4, rpm:1200` or `temp=23.4 rpm=1200`. Every key found
//!   becomes a series automatically.
//! - *Regex*: named capture groups become series.
//!
//! Key-value parsing is two-level: a line is first cut into tokens on the
//! characters that end one pair and start the next — spaces, commas, semicolons,
//! tabs — and each token is then split at the first *separator* into a name and
//! a number. A space therefore cannot be written into the separator list and
//! mean "between name and value": by the time separators are consulted it has
//! already done its other job. Putting whitespace in the list instead switches
//! on bare-name pairing, where a word names the number that follows it
//! (`temp 23.4`) — off by default, because prose like `Booting 42 modules`
//! would otherwise plot a series.
//!
//! Rules may be gated by a prefix (only lines starting with e.g. `PLOT:` are
//! parsed, and the prefix is stripped before parsing). Port scoping is applied
//! by the caller, which owns the per-port association.

use crate::config::{ExtractMode, ExtractRule};
use regex::Regex;

/// A compiled extraction rule ready to run against line text.
pub struct CompiledExtract {
    prefix: Option<String>,
    kind: Kind,
}

enum Kind {
    Kv { separators: Vec<char> },
    Regex { re: Regex, names: Vec<String> },
}

impl CompiledExtract {
    /// Compile a config rule. Returns an error string for invalid regex or a
    /// regex rule with no named groups.
    pub fn compile(rule: &ExtractRule) -> Result<CompiledExtract, String> {
        let kind = match rule.mode {
            ExtractMode::Kv => {
                let separators = rule
                    .kv_separators
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| vec![':', '=']);
                Kind::Kv { separators }
            }
            ExtractMode::Regex => {
                let pattern = rule
                    .pattern
                    .as_deref()
                    .ok_or_else(|| "regex extract rule has no pattern".to_string())?;
                let re = Regex::new(pattern).map_err(|e| e.to_string())?;
                let names: Vec<String> = re
                    .capture_names()
                    .flatten()
                    .map(|s| s.to_string())
                    .collect();
                if names.is_empty() {
                    return Err("regex extract rule has no named capture groups".to_string());
                }
                Kind::Regex { re, names }
            }
        };
        Ok(CompiledExtract {
            prefix: rule.prefix.clone().filter(|p| !p.is_empty()),
            kind,
        })
    }

    /// Extract `(series_name, value)` pairs from a line into `out`. Returns the
    /// number of pairs appended. Non-matching or gated-out lines append nothing.
    pub fn extract(&self, text: &str, out: &mut Vec<(String, f64)>) -> usize {
        let body = match &self.prefix {
            Some(prefix) => match text.trim_start().strip_prefix(prefix.as_str()) {
                Some(rest) => rest,
                None => return 0,
            },
            None => text,
        };

        let before = out.len();
        match &self.kind {
            Kind::Kv { separators } => extract_kv(body, separators, out),
            Kind::Regex { re, names } => extract_regex(body, re, names, out),
        }
        out.len() - before
    }
}

fn extract_kv(text: &str, separators: &[char], out: &mut Vec<(String, f64)>) {
    // Whitespace can never be found *inside* a token — it is what ended the
    // token — so its presence in the list means something else: pair a bare word
    // with the number after it.
    let bare_names = separators.iter().any(|c| c.is_whitespace());
    // A name that has already met its separator and is waiting for its number,
    // which is how `temp: 23.4` and `rpm = 1200` are written.
    let mut pending: Option<&str> = None;
    // The last bare word seen: a candidate name, promoted by a separator token
    // that follows it, or by the next number when `bare_names` is on.
    let mut last_word: Option<&str> = None;

    for token in text.split([',', ' ', '\t', ';']) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        // Find the first separator character. Stepping over it by its own
        // width, not by one byte: the separators come from the config file,
        // where nothing stops one from being non-ASCII, and a fixed step of
        // one would slice the token off a char boundary and panic.
        let at = token.find(|c: char| !c.is_whitespace() && separators.contains(&c));
        match at {
            Some(pos) => {
                let sep_len = token[pos..].chars().next().map_or(1, char::len_utf8);
                let key = token[..pos].trim();
                let val = token[pos + sep_len..].trim();
                // A token that *starts* with the separator names nothing itself:
                // the name is the word standing before it (`rpm = 1200`).
                let name = if key.is_empty() {
                    pending.take().or_else(|| last_word.take())
                } else {
                    Some(key)
                };
                pending = None;
                last_word = None;
                match (name, val.parse::<f64>()) {
                    (Some(name), Ok(v)) => out.push((name.to_string(), v)),
                    // `temp:` on its own — the number is the next token.
                    (Some(name), Err(_)) if val.is_empty() => pending = Some(name),
                    _ => {}
                }
            }
            None => match token.parse::<f64>() {
                Ok(v) => {
                    let name =
                        pending
                            .take()
                            .or_else(|| if bare_names { last_word.take() } else { None });
                    if let Some(name) = name {
                        out.push((name.to_string(), v));
                    }
                    last_word = None;
                }
                // A word where a number was expected breaks the pair being
                // built: `temp: none` plots nothing.
                Err(_) => {
                    pending = None;
                    last_word = Some(token);
                }
            },
        }
    }
}

fn extract_regex(text: &str, re: &Regex, names: &[String], out: &mut Vec<(String, f64)>) {
    if let Some(caps) = re.captures(text) {
        for name in names {
            if let Some(m) = caps.name(name) {
                if let Ok(v) = m.as_str().trim().parse::<f64>() {
                    out.push((name.clone(), v));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multibyte_separator_splits_on_a_char_boundary() {
        // Separators are only reachable by hand-editing the config, which
        // does not stop them being non-ASCII — and stepping over one by a
        // single byte used to land inside it and panic.
        let rule = ExtractRule {
            mode: ExtractMode::Kv,
            prefix: None,
            pattern: None,
            kv_separators: Some(vec!['\u{2192}']),
        };
        let c = CompiledExtract::compile(&rule).unwrap();
        let mut out = Vec::new();
        c.extract("temp\u{2192}23.5 rpm\u{2192}1200", &mut out);
        assert_eq!(
            out,
            vec![("temp".to_string(), 23.5), ("rpm".to_string(), 1200.0)]
        );
    }

    fn kv_rule(prefix: Option<&str>) -> ExtractRule {
        ExtractRule {
            mode: ExtractMode::Kv,
            prefix: prefix.map(|s| s.to_string()),
            pattern: None,
            kv_separators: None,
        }
    }

    #[test]
    fn kv_colon_and_equals() {
        let c = CompiledExtract::compile(&kv_rule(None)).unwrap();
        let mut out = Vec::new();
        c.extract("temp:23.4, rpm:1200", &mut out);
        assert_eq!(out, vec![("temp".into(), 23.4), ("rpm".into(), 1200.0)]);

        out.clear();
        c.extract("temp=1.5 rpm=9", &mut out);
        assert_eq!(out, vec![("temp".into(), 1.5), ("rpm".into(), 9.0)]);
    }

    #[test]
    fn kv_ignores_non_numeric() {
        let c = CompiledExtract::compile(&kv_rule(None)).unwrap();
        let mut out = Vec::new();
        c.extract("state:running temp:20", &mut out);
        assert_eq!(out, vec![("temp".into(), 20.0)]);
    }

    #[test]
    fn prefix_gates_lines() {
        let c = CompiledExtract::compile(&kv_rule(Some("PLOT:"))).unwrap();
        let mut out = Vec::new();
        c.extract("noise temp:1", &mut out);
        assert!(out.is_empty(), "line without prefix is ignored");
        c.extract("PLOT: temp:1 rpm:2", &mut out);
        assert_eq!(out, vec![("temp".into(), 1.0), ("rpm".into(), 2.0)]);
    }

    #[test]
    fn regex_named_groups() {
        let rule = ExtractRule {
            mode: ExtractMode::Regex,
            prefix: None,
            pattern: Some(r"rpm=(?P<rpm>\d+).*duty=(?P<duty>[\d.]+)".into()),
            kv_separators: None,
        };
        let c = CompiledExtract::compile(&rule).unwrap();
        let mut out = Vec::new();
        c.extract("rpm=1200 foo duty=0.75", &mut out);
        assert_eq!(out, vec![("rpm".into(), 1200.0), ("duty".into(), 0.75)]);
    }

    #[test]
    fn regex_requires_named_groups() {
        let rule = ExtractRule {
            mode: ExtractMode::Regex,
            prefix: None,
            pattern: Some(r"\d+".into()),
            kv_separators: None,
        };
        assert!(CompiledExtract::compile(&rule).is_err());
    }

    #[test]
    fn custom_separator() {
        let rule = ExtractRule {
            mode: ExtractMode::Kv,
            prefix: None,
            pattern: None,
            kv_separators: Some(vec!['|']),
        };
        let c = CompiledExtract::compile(&rule).unwrap();
        let mut out = Vec::new();
        c.extract("a|1 b|2", &mut out);
        assert_eq!(out, vec![("a".into(), 1.0), ("b".into(), 2.0)]);
    }
    fn kv_extract(rule: &ExtractRule, line: &str) -> Vec<(String, f64)> {
        let c = CompiledExtract::compile(rule).unwrap();
        let mut out = Vec::new();
        c.extract(line, &mut out);
        out
    }

    /// The way most human-readable output is actually written: the separator
    /// ends its token and the number stands on its own.
    #[test]
    fn a_value_in_the_next_token_still_pairs() {
        let rule = kv_rule(None);
        assert_eq!(
            kv_extract(&rule, "temp: 23.4, rpm = 1200"),
            vec![("temp".into(), 23.4), ("rpm".into(), 1200.0)]
        );
    }

    /// A word standing where the number should be ends the pair rather than
    /// letting the name reach past it to a later number.
    #[test]
    fn a_dangling_name_does_not_swallow_a_later_number() {
        let rule = kv_rule(None);
        assert_eq!(kv_extract(&rule, "temp: none 42"), vec![]);
    }

    /// Off by default: prose has plenty of `word number` in it, and none of it
    /// is a series.
    #[test]
    fn a_bare_word_is_not_a_name_unless_asked_for() {
        let rule = kv_rule(None);
        assert_eq!(kv_extract(&rule, "Booting 42 modules"), vec![]);
    }

    /// Whitespace in the separator list is what asks for it — a space cannot be
    /// found inside a token, since it is what ends one.
    #[test]
    fn whitespace_in_the_separators_pairs_a_word_with_the_number_after_it() {
        let rule = ExtractRule {
            mode: ExtractMode::Kv,
            prefix: None,
            pattern: None,
            kv_separators: Some(vec![':', '=', ' ']),
        };
        assert_eq!(
            kv_extract(&rule, "temp 23.4 rpm 1200"),
            vec![("temp".into(), 23.4), ("rpm".into(), 1200.0)]
        );
        // The unit after a value is a word, so it becomes the *next* candidate
        // name and is dropped when a real name follows.
        assert_eq!(
            kv_extract(&rule, "temp 23.4 C rpm 1200"),
            vec![("temp".into(), 23.4), ("rpm".into(), 1200.0)]
        );
        // Explicit pairs keep working alongside it.
        assert_eq!(
            kv_extract(&rule, "temp:23.4 rpm 1200"),
            vec![("temp".into(), 23.4), ("rpm".into(), 1200.0)]
        );
    }

    /// A clock in a log line is not a pair, in either mode.
    #[test]
    fn a_timestamp_is_not_mistaken_for_a_pair() {
        for seps in [vec![':', '='], vec![':', '=', ' ']] {
            let rule = ExtractRule {
                mode: ExtractMode::Kv,
                prefix: None,
                pattern: None,
                kv_separators: Some(seps),
            };
            assert_eq!(
                kv_extract(&rule, "12:30:00 temp:5"),
                vec![("temp".into(), 5.0)]
            );
        }
    }
}
