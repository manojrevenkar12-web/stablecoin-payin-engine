//! Sender screening against the sanctions list. A hit holds the deposit for a person to decide;
//! the engine never rejects on a name score alone.

use crate::config::ScreeningCfg;
use crate::domain::SanctionsEntry;
use crate::names::{Normaliser, similarity_tokens};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub entry_id: String,
    pub listed_name: String,
    pub score: f64,
}

pub struct Screener {
    entries: Vec<(String, String, Vec<String>)>, // (entry id, listed name, tokens)
    raw_names: Vec<String>,
    mode_exact: bool,
    threshold: f64,
}

impl Screener {
    pub fn new(list: &[SanctionsEntry], cfg: &ScreeningCfg, n: &Normaliser) -> Self {
        let mut entries = vec![];
        let mut raw_names = vec![];
        for e in list {
            for name in &e.names {
                entries.push((e.id.clone(), name.clone(), n.tokens(name)));
                raw_names.push(name.clone());
            }
        }
        Screener { entries, raw_names, mode_exact: cfg.mode == "exact", threshold: cfg.hold_threshold }
    }

    /// The best hit at or above the threshold, if any.
    ///
    /// `field_limit` is the length of the name field on the rail the payment came by (ACH company
    /// names hold 16 characters). A name that fills the field may have been cut, so it is also compared
    /// with each listed name cut to the same length.
    pub fn screen(&self, n: &Normaliser, name: &str, field_limit: Option<usize>) -> Option<Hit> {
        let t = n.tokens(name);
        if t.is_empty() {
            return None;
        }
        let cut = field_limit.filter(|l| name.trim_end().chars().count() + 2 >= *l);
        let mut best: Option<Hit> = None;
        for (k, (id, listed, lt)) in self.entries.iter().enumerate() {
            let full = if self.mode_exact {
                let mut a = t.clone();
                let mut b = lt.clone();
                a.sort();
                b.sort();
                if a == b { 1.0 } else { 0.0 }
            } else {
                similarity_tokens(&t, lt)
            };
            let score = match cut {
                Some(l) if !self.mode_exact => {
                    let truncated: String = self.raw_names[k].chars().take(l).collect();
                    full.max(similarity_tokens(&t, &n.tokens(&truncated)))
                }
                _ => full,
            };
            if score >= self.threshold && best.as_ref().is_none_or(|b| score > b.score) {
                best = Some(Hit { entry_id: id.clone(), listed_name: listed.clone(), score });
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> Vec<SanctionsEntry> {
        vec![
            SanctionsEntry { id: "S-1".into(), names: vec!["Mohammed Al-Rashid".into()], kind: "individual".into() },
            SanctionsEntry { id: "S-2".into(), names: vec!["Volga Maritime Trading LLC".into(), "Volga Marine Trade".into()], kind: "entity".into() },
        ]
    }

    #[test]
    fn fuzzy_catches_variants_exact_does_not() {
        let cfg = crate::config::load_named("engine").unwrap();
        let n = Normaliser::new(&cfg.names.variants);
        let fuzzy = Screener::new(&list(), &cfg.screening, &n);
        assert_eq!(fuzzy.screen(&n, "Muhammad Al Rashid", None).unwrap().entry_id, "S-1");
        assert_eq!(fuzzy.screen(&n, "RASHID MOHAMED AL", None).unwrap().entry_id, "S-1");
        assert_eq!(fuzzy.screen(&n, "Volga Maritime Trdg Ltd", None).unwrap().entry_id, "S-2");
        assert!(fuzzy.screen(&n, "Northwind Logistics GmbH", None).is_none());
        // An ACH company name cut at 16 characters, with a slip in it.
        assert!(fuzzy.screen(&n, "Volga Marieime T", None).is_none());
        assert_eq!(fuzzy.screen(&n, "Volga Marieime T", Some(16)).unwrap().entry_id, "S-2");

        let base = crate::config::load_named("baseline").unwrap();
        let exact = Screener::new(&list(), &base.screening, &n);
        assert!(exact.screen(&n, "Mohammed Al-Rashid", None).is_some());
        assert!(exact.screen(&n, "Muhammad Al Rashid", None).is_some(), "variant table still applies");
        assert!(exact.screen(&n, "Mohammed Al Rashed", None).is_none(), "one letter defeats exact matching");
    }
}
