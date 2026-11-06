//! Which Autoramp does a credit belong to — or must it go back to the sender?
//!
//! A deposit that matches no Approved Autoramp is returned. Returning money costs the sender fees and
//! days, and crediting the wrong customer is far worse, so the rules are: match exactly when we can,
//! read a reference the way a person would (any case, with or without the hyphen, split across lines),
//! repair a one-character typo only when exactly one live reference is that close *and* the sender is
//! the customer, and return everything else.

use crate::config::EngineConfig;
use crate::domain::{AutorampStatus, CustomerStatus, InboundPayment, Platform};
use crate::names::{Normaliser, similarity};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchHow {
    DedicatedAccount,
    ReferenceExact,
    /// Same reference once case, spaces, hyphens and line breaks are ignored.
    ReferenceNormalised,
    /// One edit away from exactly one live reference, and the sender is the customer.
    ReferenceRepaired {
        written: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReturnReason {
    UnknownAccount,
    AutorampNotApproved,
    MissingReference,
    UnknownReference,
    AmbiguousReference,
    /// A near-miss reference from someone who is not the customer: not ours to guess, even on an
    /// Autoramp that accepts third parties.
    UnverifiedSenderForRepair,
    CurrencyNotAccepted,
    CustomerSuspended,
}

impl ReturnReason {
    pub fn text(self) -> &'static str {
        match self {
            ReturnReason::UnknownAccount => "credited account is not one of ours",
            ReturnReason::AutorampNotApproved => "the Autoramp for this account or reference is not Approved",
            ReturnReason::MissingReference => "pay-by-reference account paid without a reference",
            ReturnReason::UnknownReference => "reference matches no Autoramp on this account",
            ReturnReason::AmbiguousReference => "reference is one edit away from more than one Autoramp",
            ReturnReason::UnverifiedSenderForRepair => "reference needs repair but the sender is not the customer",
            ReturnReason::CurrencyNotAccepted => "the Autoramp does not accept this currency",
            ReturnReason::CustomerSuspended => "customer is suspended",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Route {
    Matched { autoramp_id: String, how: MatchHow },
    Return { reason: ReturnReason },
}

fn compact(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

fn norm_account(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).map(|c| c.to_ascii_uppercase()).collect()
}

/// Optimal-string-alignment distance (Levenshtein plus adjacent transpositions).
pub fn osa_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=b.len() {
        d[0][j] = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

/// Words that follow the reference prefix where the prefix begins a word: "IR-7KQ2MX", "ir 7kq2mx",
/// "IR7KQ2MX". Upper-cased, separators removed.
pub fn reference_words(fields: &[&str], prefix: &str) -> Vec<String> {
    let mut out = vec![];
    for f in fields {
        let up: Vec<char> = f.to_uppercase().chars().collect();
        let pre: Vec<char> = prefix.chars().collect();
        let mut i = 0;
        while i + pre.len() <= up.len() {
            let starts_word = i == 0 || !up[i - 1].is_ascii_alphanumeric();
            if starts_word && up[i..i + pre.len()] == pre[..] {
                let mut j = i + pre.len();
                if j < up.len() && (up[j] == '-' || up[j] == ' ') {
                    j += 1;
                }
                let word: String = up[j..].iter().take_while(|c| c.is_ascii_alphanumeric()).collect();
                if !word.is_empty() {
                    out.push(word);
                }
            }
            i += 1;
        }
    }
    out
}

pub struct Router<'a> {
    pub cfg: &'a EngineConfig,
    pub platform: &'a Platform,
    pub names: &'a Normaliser,
}

impl Router<'_> {
    fn sender_is_customer(&self, p: &InboundPayment, customer_id: &str) -> bool {
        let Some(c) = self.platform.customer(customer_id) else { return false };
        std::iter::once(&c.legal_name).chain(c.trading_names.iter()).any(|n| similarity(self.names, &p.debtor_name, n) >= self.cfg.names.sender_match_threshold)
    }

    pub fn route(&self, p: &InboundPayment) -> Route {
        let acct = norm_account(&p.creditor_account);
        let on_account: Vec<_> = self.platform.autoramps.iter().filter(|a| norm_account(&a.deposit_account) == acct).collect();
        if on_account.is_empty() {
            return Route::Return { reason: ReturnReason::UnknownAccount };
        }
        let route = if let Some(dedicated) = on_account.iter().find(|a| !a.pay_by_reference) {
            if dedicated.status != AutorampStatus::Approved {
                return Route::Return { reason: ReturnReason::AutorampNotApproved };
            }
            Route::Matched { autoramp_id: dedicated.id.clone(), how: MatchHow::DedicatedAccount }
        } else {
            self.route_by_reference(p, &on_account)
        };
        if let Route::Matched { autoramp_id, .. } = &route {
            let a = self.platform.autoramp(autoramp_id).expect("matched autoramp exists");
            if a.ccy != p.ccy {
                return Route::Return { reason: ReturnReason::CurrencyNotAccepted };
            }
            if self.platform.customer(&a.customer_id).map(|c| c.status) == Some(CustomerStatus::Suspended) {
                return Route::Return { reason: ReturnReason::CustomerSuspended };
            }
        }
        route
    }

    fn route_by_reference(&self, p: &InboundPayment, on_account: &[&crate::domain::Autoramp]) -> Route {
        let rc = &self.cfg.reference;
        let fields: Vec<&str> = [p.structured_ref.as_deref(), p.unstructured.as_deref()].into_iter().flatten().collect();
        if rc.exact_only {
            let words: Vec<&str> = fields.iter().flat_map(|f| f.split_whitespace()).collect();
            for a in on_account {
                let Some(r) = &a.reference else { continue };
                if words.iter().any(|w| w == r) {
                    if a.status != AutorampStatus::Approved {
                        return Route::Return { reason: ReturnReason::AutorampNotApproved };
                    }
                    return Route::Matched { autoramp_id: a.id.clone(), how: MatchHow::ReferenceExact };
                }
            }
            let prefix = rc.prefix.trim_end_matches('-');
            let mentions = words.iter().any(|w| w.to_uppercase().contains(prefix));
            return Route::Return { reason: if mentions { ReturnReason::UnknownReference } else { ReturnReason::MissingReference } };
        }

        let text = compact(&fields.join(" "));
        let prefix = compact(&rc.prefix);
        // 1. A reference present once case, spacing, hyphens and line breaks are ignored.
        let present: Vec<_> = on_account.iter().filter(|a| a.reference.as_ref().is_some_and(|r| text.contains(&compact(r)))).collect();
        let live: Vec<_> = present.iter().filter(|a| a.status == AutorampStatus::Approved).collect();
        match live.len() {
            1 => {
                let a = live[0];
                let exact = fields.iter().any(|f| f.split_whitespace().any(|w| Some(w) == a.reference.as_deref()));
                return Route::Matched { autoramp_id: a.id.clone(), how: if exact { MatchHow::ReferenceExact } else { MatchHow::ReferenceNormalised } };
            }
            n if n > 1 => return Route::Return { reason: ReturnReason::AmbiguousReference },
            _ => {}
        }
        if !present.is_empty() {
            return Route::Return { reason: ReturnReason::AutorampNotApproved };
        }
        // 2. Candidates: the word that follows the prefix where the prefix starts a word ("IR-7KQ2MZ",
        //    "ir 7kq2mz"), taken whole. A repair is judged on the whole word, never on part of it, so a
        //    wrong last character cannot be dropped to make a match.
        let cands = reference_words(&fields, &prefix);
        if cands.is_empty() {
            return Route::Return { reason: if text.contains(&prefix) { ReturnReason::UnknownReference } else { ReturnReason::MissingReference } };
        }
        if rc.max_repair_distance == 0 {
            return Route::Return { reason: ReturnReason::UnknownReference };
        }
        let mut close: Vec<(&crate::domain::Autoramp, String)> = vec![];
        for a in on_account.iter().filter(|a| a.status == AutorampStatus::Approved) {
            let Some(r) = &a.reference else { continue };
            let body = compact(r).trim_start_matches(&prefix).to_string();
            for cand in &cands {
                if cand.len() + 1 >= rc.length
                    && cand.len() <= rc.length + 1
                    && osa_distance(cand, &body) <= rc.max_repair_distance
                    && !close.iter().any(|(x, _)| x.id == a.id)
                {
                    close.push((a, format!("{}{}", rc.prefix, cand)));
                }
            }
        }
        match close.len() {
            0 => Route::Return { reason: ReturnReason::UnknownReference },
            1 => {
                let (a, written) = &close[0];
                if rc.require_sender_match_for_repair && !self.sender_is_customer(p, &a.customer_id) {
                    return Route::Return { reason: ReturnReason::UnverifiedSenderForRepair };
                }
                Route::Matched { autoramp_id: a.id.clone(), how: MatchHow::ReferenceRepaired { written: written.clone() } }
            }
            _ => Route::Return { reason: ReturnReason::AmbiguousReference },
        }
    }

    /// Is the sender someone other than the customer, on an Autoramp that does not accept that?
    pub fn third_party_violation(&self, p: &InboundPayment, autoramp_id: &str) -> bool {
        let Some(a) = self.platform.autoramp(autoramp_id) else { return false };
        !a.third_party && !self.sender_is_customer(p, &a.customer_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::*;
    use crate::money::Ccy;

    fn platform() -> Platform {
        let cust = |id: &str, name: &str| Customer {
            id: id.into(),
            legal_name: name.into(),
            trading_names: vec![],
            status: CustomerStatus::Active,
            wallets: vec![],
            payout_account: (Rail::Sepa, "DE00".into()),
        };
        let ar = |id: &str, c: &str, acct: &str, r: Option<&str>, st: AutorampStatus| Autoramp {
            id: id.into(),
            customer_id: c.into(),
            ccy: Ccy::EUR,
            chain: "ethereum".into(),
            wallet: "0x1".into(),
            deposit_account: acct.into(),
            pay_by_reference: r.is_some(),
            reference: r.map(String::from),
            third_party: false,
            status: st,
        };
        Platform {
            customers: vec![cust("c1", "Northwind Logistics GmbH"), cust("c2", "Blue Harbor Trading Ltd")],
            autoramps: vec![
                ar("a1", "c1", "DE11 0000 0000 0000 0000 01", None, AutorampStatus::Approved),
                ar("a2", "c1", "POOL", Some("IR-7KQ2MX"), AutorampStatus::Approved),
                ar("a3", "c2", "POOL", Some("IR-7KQ2MZ"), AutorampStatus::Approved),
                ar("a4", "c2", "POOL", Some("IR-B8D4WN"), AutorampStatus::Approved),
                ar("a5", "c2", "POOL", Some("IR-CANCEL"), AutorampStatus::Cancelled),
            ],
            sanctions: vec![],
            bank_accounts: vec![],
            redemption_addresses: vec![],
        }
    }

    fn pay(acct: &str, text: Option<&str>, sender: &str) -> InboundPayment {
        InboundPayment {
            rail: Rail::Sepa,
            bank_ref: "r".into(),
            ids: TraceIds::default(),
            amount_minor: 10_000,
            ccy: Ccy::EUR,
            booked_at: 0,
            debtor_name: sender.into(),
            debtor_account: "X".into(),
            creditor_account: acct.into(),
            structured_ref: None,
            unstructured: text.map(String::from),
        }
    }

    fn route(cfg_name: &str, p: &InboundPayment) -> Route {
        let cfg = crate::config::load_named(cfg_name).unwrap();
        let pl = platform();
        let n = Normaliser::new(&cfg.names.variants);
        Router { cfg: &cfg, platform: &pl, names: &n }.route(p)
    }

    #[test]
    fn osa_counts_transpositions_as_one() {
        assert_eq!(osa_distance("7KQ2MX", "K7Q2MX"), 1);
        assert_eq!(osa_distance("7KQ2MX", "7KQ2M"), 1);
        assert_eq!(osa_distance("7KQ2MX", "7KQ2MXX"), 1);
        assert_eq!(osa_distance("7KQ2MX", "B8D4WN"), 6);
    }

    #[test]
    fn dedicated_account_matches_regardless_of_spacing() {
        let r = route("engine", &pay("DE11000000000000000001", None, "Anyone"));
        assert_eq!(r, Route::Matched { autoramp_id: "a1".into(), how: MatchHow::DedicatedAccount });
        assert_eq!(route("engine", &pay("DE99", None, "x")), Route::Return { reason: ReturnReason::UnknownAccount });
    }

    #[test]
    fn reads_references_like_a_person() {
        let exact = route("engine", &pay("POOL", Some("Invoice 4411 IR-B8D4WN"), "x"));
        assert_eq!(exact, Route::Matched { autoramp_id: "a4".into(), how: MatchHow::ReferenceExact });
        for text in ["ir-b8d4wn", "IR B8D4WN", "IRB8D4WN thanks", "ref IR-B8D4 WN"] {
            assert_eq!(
                route("engine", &pay("POOL", Some(text), "x")),
                Route::Matched { autoramp_id: "a4".into(), how: MatchHow::ReferenceNormalised },
                "{text}"
            );
        }
    }

    #[test]
    fn repairs_one_typo_only_for_the_customer_and_only_when_unambiguous() {
        // One edit from IR-B8D4WN only, sent by the customer: repaired.
        let r = route("engine", &pay("POOL", Some("IR-B8D4VN"), "BLUE HARBOR TRADING LIMITED"));
        assert!(matches!(r, Route::Matched { ref autoramp_id, how: MatchHow::ReferenceRepaired { .. } } if autoramp_id == "a4"));
        // Same typo from a stranger: returned.
        assert_eq!(route("engine", &pay("POOL", Some("IR-B8D4VN"), "Someone Else")), Route::Return { reason: ReturnReason::UnverifiedSenderForRepair });
        // One edit from both IR-7KQ2MX and IR-7KQ2MZ: returned, whoever sent it.
        assert_eq!(route("engine", &pay("POOL", Some("IR-7KQ2MA"), "Northwind Logistics")), Route::Return { reason: ReturnReason::AmbiguousReference });
        // Two edits away: returned.
        assert_eq!(route("engine", &pay("POOL", Some("IR-B8D9VN"), "Blue Harbor Trading")), Route::Return { reason: ReturnReason::UnknownReference });
    }

    #[test]
    fn cancelled_missing_and_baseline() {
        assert_eq!(route("engine", &pay("POOL", Some("IR-CANCEL"), "x")), Route::Return { reason: ReturnReason::AutorampNotApproved });
        assert_eq!(route("engine", &pay("POOL", Some("thanks for the coffee"), "x")), Route::Return { reason: ReturnReason::MissingReference });
        // The strict baseline only accepts the reference exactly as issued.
        assert_eq!(route("baseline", &pay("POOL", Some("IR-B8D4WN"), "x")), Route::Matched { autoramp_id: "a4".into(), how: MatchHow::ReferenceExact });
        assert_eq!(route("baseline", &pay("POOL", Some("ir-b8d4wn"), "x")), Route::Return { reason: ReturnReason::UnknownReference });
    }
}
