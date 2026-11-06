//! Scores a run against the world's truth. With `sim`, the only code allowed to read the truth.

use crate::engine::{Engine, TxKind};
use crate::lifecycle::Status;
use crate::money::Ccy;
use crate::routing::{MatchHow, Route};
use crate::sim::run::RunOutput;
use crate::sim::truth::{Expected, Kind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Count {
    pub n: u64,
    pub eur_minor: i64,
    pub usd_minor: i64,
}

impl Count {
    fn add(&mut self, ccy: Ccy, v: i64) {
        self.n += 1;
        match ccy {
            Ccy::EUR => self.eur_minor += v,
            Ccy::USD => self.usd_minor += v,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Rate {
    pub hit: u64,
    pub of: u64,
}

impl Rate {
    fn add(&mut self, ok: bool) {
        self.of += 1;
        if ok {
            self.hit += 1;
        }
    }
    pub fn pct(&self) -> f64 {
        if self.of == 0 { 100.0 } else { 100.0 * self.hit as f64 / self.of as f64 }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Results {
    pub policy: String,
    pub world: String,
    pub seed: u64,
    pub deposits: u64,
    pub volume: Count,
    /// Final outcome matches what should have happened.
    pub correct_outcome: Rate,
    /// Credited to an Autoramp other than the one the sender meant. Must be zero.
    pub misrouted: Count,
    /// Credited although it should have been held, rejected or returned (sanctions, third party,
    /// unverifiable reference, the customer's accidental second payment).
    pub credited_against_policy: Count,
    /// Sent back although it belonged to a customer.
    pub wrongly_returned: Count,
    /// Belonged to a customer and reached them.
    pub credited_when_due: Rate,
    pub repaired_references: u64,
    pub normalised_references: u64,
    /// The customer's accidental second payment minted a second time.
    pub duplicate_mints: Count,
    pub resends_held: Rate,
    pub sanctions_held: Rate,
    pub sanctions_variants_held: Rate,
    pub third_party_held: Rate,
    pub edd_held: Rate,
    /// Holds a person had to clear although nothing was wrong, by reason.
    pub false_holds: BTreeMap<String, u64>,
    pub false_holds_per_1000: f64,
    pub below_minimum_rejected: Rate,
    /// Deposits findable by every ID the sender could quote.
    pub trace_all: Rate,
    pub trace_returned: Rate,
    pub statement_only_recovered: Rate,
    pub renotifications_ignored: Rate,
    pub corrupt_files_quarantined: Rate,
    pub deposits_booked_once: Rate,
    pub reorg_dropped_mints: u64,
    pub reorg_reincluded_mints: u64,
    pub mint_resubmissions: u64,
    /// Completed mints whose op is final on-chain exactly once.
    pub mints_final_once: Rate,
    pub reorgs_past_confirmation: u64,
    pub trial_balance_zero: bool,
    pub min_reserve_headroom_minor: BTreeMap<String, i64>,
    pub recon_days: u64,
    pub fees_found: Rate,
    pub unexplained_balance_breaks: u64,
    pub chain_supply_breaks: u64,
    pub reserve_coverage_breaks: u64,
    pub usd_conversions_outside_window: u64,
    pub payout_returns_processed: Rate,
    pub redemptions_completed: u64,
    pub hours_to_mint_p50: BTreeMap<String, f64>,
    pub hours_to_mint_p90: BTreeMap<String, f64>,
    pub holds_opened: u64,
    pub files_processed: u64,
    pub inputs: u64,
    pub engine_ms: u128,
    pub inputs_per_sec: f64,
    /// Planted cases by kind: (count, correct outcome).
    pub by_kind: BTreeMap<String, (u64, u64)>,
}

fn pct(v: &mut [f64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let i = ((v.len() as f64 - 1.0) * q).round() as usize;
    (v[i] * 10.0).round() / 10.0
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Credited(String),
    Returned,
    Rejected,
    Held,
    BelowMin,
    Missing,
}

fn outcome(e: &Engine, rail: crate::domain::Rail, bank_ref: &str) -> (Outcome, Vec<String>) {
    let Some(d) = e.state.deposits.iter().find(|d| d.payment.rail == rail && d.payment.bank_ref == bank_ref) else { return (Outcome::Missing, vec![]) };
    let holds: Vec<String> = d
        .transaction_id
        .as_ref()
        .and_then(|t| e.transaction(t))
        .and_then(|t| t.hold_id.as_ref())
        .and_then(|h| e.hold(h))
        .map(|h| h.reasons.iter().map(|r| r.code().to_string()).collect())
        .unwrap_or_default();
    let o = match (&d.route, d.transaction_id.as_ref().and_then(|t| e.transaction(t))) {
        (Route::Return { .. }, _) => Outcome::Returned,
        (Route::Matched { autoramp_id, .. }, Some(t)) => match t.status {
            Status::Returned => Outcome::Returned,
            Status::RejectedMinAmount => Outcome::BelowMin,
            Status::RejectedAml | Status::RejectedFraud => Outcome::Rejected,
            Status::FundsReviewInProgress => Outcome::Held,
            _ => Outcome::Credited(autoramp_id.clone()),
        },
        _ => Outcome::Missing,
    };
    (o, holds)
}

pub fn evaluate(out: &RunOutput, world: &str, seed: u64) -> Results {
    let e = &out.engine;
    let t = &out.truth;
    let mut r = Results { policy: e.cfg.policy_name.clone(), world: world.into(), seed, ..Default::default() };
    let mut mint_hours: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for d in &t.deposits {
        r.deposits += 1;
        r.volume.add(d.ccy, d.amount_minor);
        let (o, holds) = outcome(e, d.rail, &d.bank_ref);
        let decision = d.review_decision.as_deref();
        let should = match d.expected {
            Expected::Credit => Outcome::Credited(d.intended_autoramp.clone().unwrap_or_default()),
            Expected::Return => Outcome::Returned,
            Expected::RejectMin => Outcome::BelowMin,
            Expected::Hold => match decision {
                Some("reject") => Outcome::Rejected,
                Some("return") => Outcome::Returned,
                _ => Outcome::Credited(d.intended_autoramp.clone().unwrap_or_default()),
            },
        };
        let ok = o == should;
        r.correct_outcome.add(ok);
        let k = format!("{:?}", d.kind);
        let ent = r.by_kind.entry(k).or_default();
        ent.0 += 1;
        if ok {
            ent.1 += 1;
        }
        let due = matches!(should, Outcome::Credited(_));
        if due {
            r.credited_when_due.add(o == should);
        }
        if let Outcome::Credited(a) = &o {
            if d.intended_autoramp.as_deref() != Some(a.as_str()) {
                r.misrouted.add(d.ccy, d.amount_minor);
            } else if !matches!(should, Outcome::Credited(_)) {
                r.credited_against_policy.add(d.ccy, d.amount_minor);
            }
        }
        if o == Outcome::Returned && matches!(should, Outcome::Credited(_)) {
            r.wrongly_returned.add(d.ccy, d.amount_minor);
        }
        if d.kind == Kind::CustomerResend && d.resend_of.is_some() {
            r.resends_held.add(holds.iter().any(|h| h == "possible_resend"));
            if matches!(o, Outcome::Credited(_)) {
                r.duplicate_mints.add(d.ccy, d.amount_minor);
            }
        }
        if matches!(d.kind, Kind::SanctionedExact | Kind::SanctionedVariant) {
            let held = holds.iter().any(|h| h == "sanctions_hit");
            r.sanctions_held.add(held);
            if d.kind == Kind::SanctionedVariant {
                r.sanctions_variants_held.add(held);
            }
        }
        if d.kind == Kind::ThirdPartyNotAllowed {
            r.third_party_held.add(holds.iter().any(|h| h == "third_party_sender"));
        }
        if d.expected_holds.iter().any(|h| h == "customer_in_edd") {
            r.edd_held.add(holds.iter().any(|h| h == "customer_in_edd"));
        }
        if d.kind == Kind::BelowMinimum {
            r.below_minimum_rejected.add(o == Outcome::BelowMin);
        }
        for h in &holds {
            if !d.expected_holds.contains(h) {
                *r.false_holds.entry(h.clone()).or_default() += 1;
            }
        }
        // Trace by every ID the sender could quote.
        let found = !d.trace_ids.is_empty() && d.trace_ids.iter().all(|id| e.trace(id).is_some_and(|x| x.payment.bank_ref == d.bank_ref));
        r.trace_all.add(found);
        if o == Outcome::Returned {
            r.trace_returned.add(found);
        }
        if d.statement_only {
            r.statement_only_recovered.add(o != Outcome::Missing);
        }
        let booked = e.state.deposits.iter().filter(|x| x.payment.rail == d.rail && x.payment.bank_ref == d.bank_ref).count();
        r.deposits_booked_once.add(booked == 1 || (booked == 0 && d.statement_only && !e.cfg.ingest_statement_only_credits));
        // Time from booking to final mint, for deposits that went straight through.
        if let Some(dep) = e.state.deposits.iter().find(|x| x.payment.rail == d.rail && x.payment.bank_ref == d.bank_ref) {
            if let Some(tx) = dep.transaction_id.as_ref().and_then(|t| e.transaction(t)) {
                if tx.status == Status::Completed && tx.hold_id.is_none() {
                    if let Some((at, _, _)) = tx.history.iter().find(|(_, s, _)| *s == Status::Completed) {
                        mint_hours.entry(d.ccy.code().to_string()).or_default().push((*at - dep.payment.booked_at) as f64 / 3600.0);
                    }
                }
                if tx.ccy == Ccy::USD {
                    if let Some(c) = tx.converted_at {
                        if !crate::time::in_usd_mint_window(c, e.cfg.usd_window_open_hour_et, e.cfg.usd_window_close_hour_et) {
                            r.usd_conversions_outside_window += 1;
                        }
                    }
                }
            }
        }
    }
    for (k, mut v) in mint_hours {
        r.hours_to_mint_p50.insert(k.clone(), pct(&mut v, 0.5));
        r.hours_to_mint_p90.insert(k, pct(&mut v, 0.9));
    }
    let n_holds = e.state.holds.len() as u64;
    r.holds_opened = n_holds;
    r.false_holds_per_1000 = (r.false_holds.values().sum::<u64>() as f64 * 1000.0 / r.deposits.max(1) as f64 * 10.0).round() / 10.0;
    for d in &e.state.deposits {
        match &d.route {
            Route::Matched { how: MatchHow::ReferenceRepaired { .. }, .. } => r.repaired_references += 1,
            Route::Matched { how: MatchHow::ReferenceNormalised, .. } => r.normalised_references += 1,
            _ => {}
        }
    }
    r.renotifications_ignored = Rate { hit: e.state.ignored_renotifications.min(t.renotified), of: t.renotified };
    let quarantined: Vec<&str> = e.state.quarantined.iter().map(|q| q.name.as_str()).collect();
    r.corrupt_files_quarantined =
        Rate { hit: t.corrupt_files.iter().filter(|c| quarantined.contains(&c.as_str())).count() as u64, of: t.corrupt_files.len() as u64 };
    r.reorg_dropped_mints = t.reorg_dropped_mints;
    r.reorg_reincluded_mints = t.reorg_reincluded_mints;
    for tx in e.state.transactions.iter().filter(|x| x.kind == TxKind::Mint) {
        if let Some(m) = &tx.mint {
            r.mint_resubmissions += (m.attempts - 1) as u64;
            if tx.status == Status::Completed || tx.status == Status::Failed {
                r.mints_final_once.add(e.state.final_ops.contains_key(&m.id));
            }
        }
    }
    r.reorgs_past_confirmation = e.indexer_stats().iter().map(|(_, s)| s.reorgs_past_confirmation).sum();
    r.trial_balance_zero = e.state.ledger.trial_balance().values().all(|v| *v == 0);
    for (c, v) in &e.state.ledger.min_headroom {
        r.min_reserve_headroom_minor.insert(c.code().to_string(), *v);
    }
    r.recon_days = e.state.recon.len() as u64;
    let breaks = e.state.recon.iter().flat_map(|x| x.breaks.iter());
    let mut fees = 0;
    for b in breaks {
        use crate::engine::BreakKind::*;
        match b.kind {
            BankFeeNotBooked => fees += 1,
            UnexplainedBalance => r.unexplained_balance_breaks += 1,
            ChainSupplyMismatch => r.chain_supply_breaks += 1,
            ReserveCoverage => r.reserve_coverage_breaks += 1,
            _ => {}
        }
    }
    r.fees_found = Rate { hit: fees.min(t.fees), of: t.fees };
    let failed = e.state.transactions.iter().filter(|x| x.kind == TxKind::Redeem && x.status == Status::Failed).count() as u64;
    r.payout_returns_processed = Rate { hit: failed.min(t.payout_returns), of: t.payout_returns };
    r.redemptions_completed =
        e.state.transactions.iter().filter(|x| x.kind == TxKind::Redeem && matches!(x.status, Status::Completed | Status::Failed)).count() as u64;
    r.files_processed = e.state.files_processed;
    r.inputs = out.stats.inputs;
    r.engine_ms = out.stats.engine_ms;
    r.inputs_per_sec = (out.stats.inputs as f64 / (out.stats.engine_ms.max(1) as f64 / 1000.0)).round();
    r
}
