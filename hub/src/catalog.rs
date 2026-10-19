//! The optional signal catalog, `signals.yaml` at the repo root:
//! address, unit, label. The monitor uses it for labels; nothing
//! depends on it being present or complete.
//!
//! The file is a flat list, so this is a deliberately minimal line
//! parser rather than a YAML dependency: entries start at `- address:`,
//! the keys read are `label` and `unit`, everything else is ignored. A
//! `{…}` segment (`{dev}`, `{n}`) in a catalog address matches any single
//! segment, matching the device apps' catalog readers.

use std::path::Path;

#[derive(Debug)]
struct Entry {
    segments: Vec<String>,
    label: String,
}

#[derive(Debug, Default)]
pub struct Catalog {
    entries: Vec<Entry>,
}

impl Catalog {
    /// Load `signals.yaml` if it exists; an absent or unreadable file is
    /// an empty catalog, never an error.
    pub fn load(path: &Path) -> Catalog {
        match std::fs::read_to_string(path) {
            Ok(text) => Catalog::parse(&text),
            Err(_) => Catalog::default(),
        }
    }

    /// Look in the current directory and then upward, so the monitor
    /// finds the repo root's catalog when started from anywhere inside.
    pub fn find() -> Catalog {
        let mut dir = std::env::current_dir().unwrap_or_default();
        loop {
            let candidate = dir.join("signals.yaml");
            if candidate.is_file() {
                return Catalog::load(&candidate);
            }
            if !dir.pop() {
                return Catalog::default();
            }
        }
    }

    pub fn parse(text: &str) -> Catalog {
        let mut entries = Vec::new();
        let mut address: Option<String> = None;
        let mut label: Option<String> = None;
        let mut unit: Option<String> = None;
        let flush = |address: &mut Option<String>, label: &mut Option<String>, unit: &mut Option<String>, entries: &mut Vec<Entry>| {
            if let Some(a) = address.take() {
                let text = match (label.take(), unit.take()) {
                    (Some(l), Some(u)) => format!("{l} ({u})"),
                    (Some(l), None) => l,
                    (None, Some(u)) => format!("({u})"),
                    (None, None) => return,
                };
                entries.push(Entry {
                    segments: a.trim_start_matches('/').split('/').map(str::to_owned).collect(),
                    label: text,
                });
            }
        };
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("- address:") {
                flush(&mut address, &mut label, &mut unit, &mut entries);
                address = Some(unquote(rest));
            } else if let Some(rest) = trimmed.strip_prefix("label:") {
                label = Some(unquote(rest));
            } else if let Some(rest) = trimmed.strip_prefix("unit:") {
                unit = Some(unquote(rest));
            }
        }
        flush(&mut address, &mut label, &mut unit, &mut entries);
        Catalog { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The label for an address, `{…}` catalog segments matching any
    /// single address segment.
    pub fn label(&self, address: &str) -> Option<&str> {
        let segments: Vec<&str> = address.trim_start_matches('/').split('/').collect();
        self.entries
            .iter()
            .find(|e| {
                e.segments.len() == segments.len()
                    && e.segments
                        .iter()
                        .zip(&segments)
                        .all(|(pat, seg)| {
                            (pat.starts_with('{') && pat.ends_with('}')) || pat == seg
                        })
            })
            .map(|e| e.label.as_str())
    }
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').trim_matches('\'').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# tussen signal catalog
signals:
  - address: /tussen/{dev}/head/n/yaw
    unit: 0..1
    label: head yaw, normalized
  - address: /tussen/{dev}/head/pos/x
    unit: m
    label: head x
  - address: /tussen/midi/cc/74
    label: filter cutoff
"#;

    #[test]
    fn labels_resolve_with_dev_wildcards() {
        let c = Catalog::parse(SAMPLE);
        assert_eq!(c.label("/tussen/quest/head/n/yaw"), Some("head yaw, normalized (0..1)"));
        assert_eq!(c.label("/tussen/editor/head/pos/x"), Some("head x (m)"));
        assert_eq!(c.label("/tussen/midi/cc/74"), Some("filter cutoff"));
        assert_eq!(c.label("/tussen/quest/head/n/pitch"), None);
        assert_eq!(c.label("/tussen/quest/head/n/yaw/extra"), None);
    }

    #[test]
    fn the_shipped_catalog_parses_and_covers_the_head_addresses() {
        let c = Catalog::parse(include_str!("../../signals.yaml"));
        assert!(!c.is_empty());
        assert!(c.label("/tussen/midi/note/60/pressure").is_some());
        for a in ["pos/x", "pos/y", "pos/z", "yaw", "pitch", "roll", "n/yaw", "n/height", "n/speed", "n/turn"] {
            let address = format!("/tussen/quest/head/{a}");
            assert!(c.label(&address).is_some(), "no label for {address}");
        }
    }
}
