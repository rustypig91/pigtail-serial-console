//! Numeric-series extraction rules (spec §7.13).
//!
//! Two modes:
//! - *Key-value*: `temp:23.4, rpm:1200` or `temp=23.4 rpm=1200`. Every key found
//!   becomes a series automatically.
//! - *Regex*: named capture groups become series.
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
    // Split on whitespace and commas; each token is `key<sep>value`.
    for token in text.split([',', ' ', '\t', ';']) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        // Find the first separator character. Stepping over it by its own
        // width, not by one byte: the separators come from the config file,
        // where nothing stops one from being non-ASCII, and a fixed step of
        // one would slice the token off a char boundary and panic.
        if let Some(pos) = token.find(|c| separators.contains(&c)) {
            let sep_len = token[pos..].chars().next().map_or(1, char::len_utf8);
            let key = token[..pos].trim();
            let val = token[pos + sep_len..].trim();
            if key.is_empty() {
                continue;
            }
            if let Ok(v) = val.parse::<f64>() {
                out.push((key.to_string(), v));
            }
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
}
