//! The engine: bank files, chain blocks and operations decisions in; ledger journals, transaction
//! statuses and instructions (returns, reserve sweeps, mints, burns, payouts) out.
//!
//! It is a deterministic state machine. The same inputs in the same order give the same ledger, byte
//! for byte, which is what lets the Postgres event log rebuild it and lets tests replay it.

use crate::chain::{Indexer, Log, LogKind};
use crate::config::EngineConfig;
use crate::domain::{CustomerStatus, InboundPayment, PayoutReturn, Platform, Rail};
use crate::ledger::{Ledger, acct, cr, dr};
use crate::lifecycle::{self, Status};
use crate::money::{Ccy, minor_to_base_units};
use crate::names::Normaliser;
use crate::rails::{
    self, FileKind, Parsed,
    iso::{EntryKind, Statement},
};
use crate::routing::{Route, Router};
use crate::screening::{Hit, Screener};
use crate::time::{HOUR, Ts, in_usd_mint_window};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Input {
    /// Any bank file: camt.054, camt.053, pacs.008, pacs.004, NACHA, Fedwire.
    BankFile {
        name: String,
        raw: String,
    },
    ChainBlock(crate::chain::Block),
    Decision {
        hold_id: String,
        decision: Decision,
        by: String,
    },
    /// Time passes: open mint windows, re-submit stuck mints.
    Tick,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Release,
    /// Fails AML review: funds blocked.
    Reject,
    /// Send the money back to its sender.
    Return,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum HoldReason {
    Sanctions(Hit),
    ThirdPartySender { sender: String },
    PossibleResend { of_transaction: String },
    CustomerInEdd,
    UnregisteredWallet { wallet: String },
}

impl HoldReason {
    pub fn code(&self) -> &'static str {
        match self {
            HoldReason::Sanctions(_) => "sanctions_hit",
            HoldReason::ThirdPartySender { .. } => "third_party_sender",
            HoldReason::PossibleResend { .. } => "possible_resend",
            HoldReason::CustomerInEdd => "customer_in_edd",
            HoldReason::UnregisteredWallet { .. } => "unregistered_wallet",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hold {
    pub id: String,
    pub transaction_id: String,
    pub reasons: Vec<HoldReason>,
    pub opened_at: Ts,
    pub decided: Option<(Ts, Decision, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DepositSource {
    Notification,
    /// Found only on the end-of-day statement: the bank never sent a notification.
    StatementOnly,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Deposit {
    pub id: String,
    pub payment: InboundPayment,
    pub received_at: Ts,
    pub source: DepositSource,
    pub route: Route,
    pub transaction_id: Option<String>,
    /// When a return was instructed, and its reason.
    pub returned: Option<(Ts, String, String)>,
    pub renotified: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TxKind {
    Mint,
    Redeem,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MintOp {
    pub id: String,
    pub chain: String,
    pub to: String,
    pub amount_base: i128,
    pub submitted_at: Ts,
    pub attempts: u32,
    pub tx_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transaction {
    pub id: String,
    pub kind: TxKind,
    pub deposit_id: Option<String>,
    pub autoramp_id: Option<String>,
    pub customer_id: String,
    pub ccy: Ccy,
    pub amount_minor: i64,
    pub status: Status,
    pub history: Vec<(Ts, Status, String)>,
    pub hold_id: Option<String>,
    pub mint: Option<MintOp>,
    /// Redeem: payout reference and burn ID.
    pub payout_ref: Option<String>,
    pub burn_id: Option<String>,
    /// When conversion actually ran (for the mint-window check).
    pub converted_at: Option<Ts>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Effect {
    ReturnPayment { return_id: String, deposit_id: String, rail: Rail, ccy: Ccy, amount_minor: i64, to_account: String, original_ref: String },
    ReserveSweep { sweep_id: String, ccy: Ccy, amount_minor: i64 },
    Mint { mint_id: String, chain: String, token: String, to: String, amount_base: i128 },
    Burn { burn_id: String, chain: String, token: String, amount_base: i128 },
    Payout { payout_ref: String, transaction_id: String, rail: Rail, ccy: Ccy, amount_minor: i64, to_account: String },
    HoldOpened { hold_id: String, transaction_id: String, reasons: Vec<String> },
    Webhook { transaction_id: String, status: Status },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BreakKind {
    /// Credit on the statement that never came with a notification.
    MissingNotification,
    BankFeeNotBooked,
    UnknownStatementEntry,
    /// Statement closing balance minus ledger balance, after the explained breaks.
    UnexplainedBalance,
    /// Ledger tokens outstanding differ from confirmed on-chain supply.
    ChainSupplyMismatch,
    /// The bank's reserve balance does not cover on-chain supply plus pending mints.
    ReserveCoverage,
    MintStuck,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Break {
    pub kind: BreakKind,
    pub account: String,
    pub ccy: Ccy,
    pub amount_minor: i64,
    pub reference: String,
    pub explanation: String,
    /// The engine fixed it (e.g. booked the missing deposit).
    pub resolved: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReconReport {
    pub date: String,
    pub statements: usize,
    pub entries_matched: usize,
    pub breaks: Vec<Break>,
    /// Per currency: bank reserve balance, on-chain supply, pending mints (minor units).
    pub coverage: Vec<(Ccy, i64, i64, i64)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuarantinedFile {
    pub name: String,
    pub at: Ts,
    pub error: String,
}

/// What the engine has done; everything a trace, a dashboard or an auditor needs.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub deposits: Vec<Deposit>,
    pub transactions: Vec<Transaction>,
    pub holds: Vec<Hold>,
    pub recon: Vec<ReconReport>,
    pub quarantined: Vec<QuarantinedFile>,
    pub ledger: Ledger,
    /// Every chain op seen final: op ID → (chain, tx hash, kind, amount).
    pub final_ops: BTreeMap<String, (String, String, String, i128)>,
    pub confirmed_supply: BTreeMap<String, i128>,
    /// Burns instructed whose block is not final yet, per token (base units).
    pub pending_burns: BTreeMap<String, i128>,
    pub payout_returns: Vec<PayoutReturn>,
    pub ignored_renotifications: u64,
    /// Statement items not in the ledger, carried from day to day until someone books them:
    /// account → (bank reference → signed amount).
    pub open_items: BTreeMap<String, BTreeMap<String, i64>>,
    pub files_processed: u64,
}

pub struct Engine {
    pub cfg: EngineConfig,
    pub platform: Platform,
    names: Normaliser,
    screener: Screener,
    pub state: State,
    pub outbox: Vec<Effect>,
    by_bank_ref: HashMap<(Rail, String), usize>,
    trace: HashMap<String, usize>,
    tx_index: HashMap<String, usize>,
    hold_index: HashMap<String, usize>,
    mint_index: HashMap<String, usize>,
    payout_index: HashMap<String, usize>,
    return_index: HashMap<String, usize>,
    sweep_refs: HashSet<String>,
    indexers: BTreeMap<String, Indexer>,
    /// Conversions waiting for the USD window.
    queued: Vec<String>,
    seq: u64,
    now: Ts,
}

impl Engine {
    pub fn new(cfg: EngineConfig, platform: Platform) -> Self {
        let names = Normaliser::new(&cfg.names.variants);
        let screener = Screener::new(&platform.sanctions, &cfg.screening, &names);
        let indexers = cfg.chains.iter().map(|c| (c.name.clone(), Indexer::new(&c.name, c.confirmations))).collect();
        Engine {
            cfg,
            platform,
            names,
            screener,
            state: State::default(),
            outbox: vec![],
            by_bank_ref: HashMap::new(),
            trace: HashMap::new(),
            tx_index: HashMap::new(),
            hold_index: HashMap::new(),
            mint_index: HashMap::new(),
            payout_index: HashMap::new(),
            return_index: HashMap::new(),
            sweep_refs: HashSet::new(),
            indexers,
            queued: vec![],
            seq: 0,
            now: 0,
        }
    }

    fn next(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!("{prefix}-{:06}", self.seq)
    }

    pub fn drain(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.outbox)
    }

    pub fn indexer_stats(&self) -> Vec<(String, crate::chain::IndexerStats)> {
        self.indexers.iter().map(|(k, v)| (k.clone(), v.stats.clone())).collect()
    }

    // -----------------------------------------------------------------------------------------
    // Lookups

    pub fn transaction(&self, id: &str) -> Option<&Transaction> {
        self.tx_index.get(id).map(|&i| &self.state.transactions[i])
    }

    pub fn hold(&self, id: &str) -> Option<&Hold> {
        self.hold_index.get(id).map(|&i| &self.state.holds[i])
    }

    /// Find a deposit by any identifier a sender or bank might quote: UETR, end-to-end ID, the bank's
    /// reference, ACH trace number, IMAD, or our own deposit ID.
    pub fn trace(&self, id: &str) -> Option<&Deposit> {
        let k = id.trim().to_lowercase();
        self.trace.get(&k).map(|&i| &self.state.deposits[i])
    }

    // -----------------------------------------------------------------------------------------

    pub fn handle(&mut self, at: Ts, input: Input) -> Result<()> {
        self.now = self.now.max(at);
        match input {
            Input::BankFile { name, raw } => self.on_file(at, &name, &raw),
            Input::ChainBlock(b) => self.on_block(at, b),
            Input::Decision { hold_id, decision, by } => self.on_decision(at, &hold_id, decision, &by),
            Input::Tick => {
                self.on_tick(at);
                Ok(())
            }
        }
    }

    fn on_file(&mut self, at: Ts, name: &str, raw: &str) -> Result<()> {
        // A file that does not parse is quarantined whole: never half-booked.
        let (kind, parsed) = match rails::parse(raw) {
            Ok(x) => x,
            Err(e) => {
                self.state.quarantined.push(QuarantinedFile { name: name.to_string(), at, error: format!("{e:#}") });
                return Ok(());
            }
        };
        self.state.files_processed += 1;
        match parsed {
            Parsed::Payments { credits, returns } => {
                for c in credits {
                    self.on_credit(at, c, DepositSource::Notification);
                }
                for r in returns {
                    self.on_payout_return(at, r);
                }
            }
            Parsed::Statements(stmts) => {
                debug_assert_eq!(kind, FileKind::Camt053);
                self.reconcile(at, stmts);
            }
        }
        Ok(())
    }

    fn index_trace(&mut self, i: usize) {
        let d = &self.state.deposits[i];
        let mut keys: Vec<String> = d.payment.ids.all().into_iter().map(|(_, v)| v.to_lowercase()).collect();
        keys.push(d.payment.bank_ref.to_lowercase());
        keys.push(d.id.to_lowercase());
        for k in keys {
            self.trace.entry(k).or_insert(i);
        }
    }

    fn on_credit(&mut self, at: Ts, p: InboundPayment, source: DepositSource) {
        // Technical duplicate: the bank re-sent a notification it already sent.
        if let Some(&i) = self.by_bank_ref.get(&(p.rail, p.bank_ref.clone())) {
            self.state.deposits[i].renotified += 1;
            self.state.ignored_renotifications += 1;
            return;
        }
        let id = self.next("dep");
        let route = Router { cfg: &self.cfg, platform: &self.platform, names: &self.names }.route(&p);
        let ccy = p.ccy;
        let amount = p.amount_minor;
        let booked = p.booked_at;
        let i = self.state.deposits.len();
        self.state.deposits.push(Deposit {
            id: id.clone(),
            payment: p,
            received_at: at,
            source,
            route: route.clone(),
            transaction_id: None,
            returned: None,
            renotified: 0,
        });
        self.by_bank_ref.insert((self.state.deposits[i].payment.rail, self.state.deposits[i].payment.bank_ref.clone()), i);

        match route {
            Route::Return { reason } => {
                // Money arrived but belongs to no Autoramp: owed back to the sender.
                self.post(booked, "deposit_unmatched", &id, vec![dr(acct::operating(ccy), ccy, amount), cr(acct::returns_due(ccy), ccy, amount)]);
                self.instruct_return(at, i, reason.text());
                if self.cfg.trace_returned_deposits {
                    self.index_trace(i);
                }
            }
            Route::Matched { autoramp_id, .. } => {
                self.index_trace(i);
                let ar = self.platform.autoramp(&autoramp_id).cloned().expect("routed to a known autoramp");
                self.post(booked, "deposit", &id, vec![dr(acct::operating(ccy), ccy, amount), cr(acct::held(&ar.customer_id, ccy), ccy, amount)]);
                let tx_id = self.next("tx");
                let tx = Transaction {
                    id: tx_id.clone(),
                    kind: TxKind::Mint,
                    deposit_id: Some(id.clone()),
                    autoramp_id: Some(autoramp_id.clone()),
                    customer_id: ar.customer_id.clone(),
                    ccy,
                    amount_minor: amount,
                    status: Status::FundsReviewInProgress,
                    history: vec![(at, Status::FundsReviewInProgress, "deposit received".into())],
                    hold_id: None,
                    mint: None,
                    payout_ref: None,
                    burn_id: None,
                    converted_at: None,
                };
                self.tx_index.insert(tx_id.clone(), self.state.transactions.len());
                self.state.transactions.push(tx);
                self.state.deposits[i].transaction_id = Some(tx_id.clone());
                self.outbox.push(Effect::Webhook { transaction_id: tx_id.clone(), status: Status::FundsReviewInProgress });

                if amount < self.cfg.minimum(ccy) {
                    self.set_status(at, &tx_id, Status::RejectedMinAmount, "below the minimum");
                    self.post(at, "below_minimum", &id, vec![dr(acct::held(&ar.customer_id, ccy), ccy, amount), cr(acct::returns_due(ccy), ccy, amount)]);
                    self.instruct_return(at, i, "below the minimum amount");
                    return;
                }
                let reasons = self.checks(i, &ar);
                if reasons.is_empty() {
                    self.set_status(at, &tx_id, Status::ConversionInProgress, "checks passed");
                    self.convert_or_queue(at, &tx_id);
                } else {
                    self.open_hold(at, &tx_id, reasons);
                }
            }
        }
    }

    /// Compliance and fraud checks on a matched deposit. Each failing check is a reason to hold.
    fn checks(&self, i: usize, ar: &crate::domain::Autoramp) -> Vec<HoldReason> {
        let d = &self.state.deposits[i];
        let p = &d.payment;
        let mut reasons = vec![];
        let limit = self.cfg.screening.name_field_limits.iter().find(|(r, _)| *r == p.rail).map(|(_, l)| *l);
        if let Some(hit) = self.screener.screen(&self.names, &p.debtor_name, limit) {
            reasons.push(HoldReason::Sanctions(hit));
        }
        if self.platform.customer(&ar.customer_id).map(|c| c.status) == Some(CustomerStatus::Edd) {
            reasons.push(HoldReason::CustomerInEdd);
        }
        let router = Router { cfg: &self.cfg, platform: &self.platform, names: &self.names };
        if self.cfg.hold_third_party_senders && router.third_party_violation(p, &ar.id) {
            reasons.push(HoldReason::ThirdPartySender { sender: p.debtor_name.clone() });
        }
        if self.cfg.resend.enabled {
            let window = self.cfg.resend.window_hours * HOUR;
            let acct_key: String = p.debtor_account.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_uppercase();
            let prior = self.state.deposits[..i].iter().rev().take_while(|o| p.booked_at - o.payment.booked_at <= window).find(|o| {
                o.transaction_id.is_some()
                    && o.payment.amount_minor == p.amount_minor
                    && o.payment.ccy == p.ccy
                    && o.payment.debtor_account.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_uppercase() == acct_key
                    && matches!(&o.route, Route::Matched { autoramp_id, .. } if *autoramp_id == ar.id)
            });
            if let Some(o) = prior {
                reasons.push(HoldReason::PossibleResend { of_transaction: o.transaction_id.clone().unwrap() });
            }
        }
        reasons
    }

    fn open_hold(&mut self, at: Ts, tx_id: &str, reasons: Vec<HoldReason>) {
        let hold_id = self.next("hold");
        let codes = reasons.iter().map(|r| r.code().to_string()).collect();
        self.hold_index.insert(hold_id.clone(), self.state.holds.len());
        self.state.holds.push(Hold { id: hold_id.clone(), transaction_id: tx_id.to_string(), reasons, opened_at: at, decided: None });
        let i = self.tx_index[tx_id];
        self.state.transactions[i].hold_id = Some(hold_id.clone());
        self.state.transactions[i].history.push((at, Status::FundsReviewInProgress, format!("held: {hold_id}")));
        self.outbox.push(Effect::HoldOpened { hold_id, transaction_id: tx_id.to_string(), reasons: codes });
    }

    fn set_status(&mut self, at: Ts, tx_id: &str, to: Status, note: &str) {
        let i = self.tx_index[tx_id];
        let from = self.state.transactions[i].status;
        lifecycle::check(from, to).unwrap_or_else(|e| panic!("{tx_id}: {e}"));
        let t = &mut self.state.transactions[i];
        t.status = to;
        t.history.push((at, to, note.to_string()));
        self.outbox.push(Effect::Webhook { transaction_id: tx_id.to_string(), status: to });
    }

    fn post(&mut self, at: Ts, kind: &str, reference: &str, postings: Vec<crate::ledger::Posting>) {
        // A refused journal is a bug in the engine, not a business outcome: stop loudly.
        self.state.ledger.post(at, kind, reference, postings).unwrap_or_else(|e| panic!("journal {kind} for {reference} refused: {e}"));
    }

    fn instruct_return(&mut self, at: Ts, dep_idx: usize, reason: &str) {
        let return_id = self.next("ret");
        let d = &self.state.deposits[dep_idx];
        let (ccy, amount) = (d.payment.ccy, d.payment.amount_minor);
        let eff = Effect::ReturnPayment {
            return_id: return_id.clone(),
            deposit_id: d.id.clone(),
            rail: d.payment.rail,
            ccy,
            amount_minor: amount,
            to_account: d.payment.debtor_account.clone(),
            original_ref: d.payment.bank_ref.clone(),
        };
        self.state.deposits[dep_idx].returned = Some((at, reason.to_string(), return_id.clone()));
        self.return_index.insert(return_id.clone(), dep_idx);
        self.post(at, "return_sent", &return_id, vec![dr(acct::returns_due(ccy), ccy, amount), cr(acct::operating(ccy), ccy, amount)]);
        self.outbox.push(eff);
    }

    fn window_open(&self, ccy: Ccy, at: Ts) -> bool {
        ccy != Ccy::USD || in_usd_mint_window(at, self.cfg.usd_window_open_hour_et, self.cfg.usd_window_close_hour_et)
    }

    fn convert_or_queue(&mut self, at: Ts, tx_id: &str) {
        let ccy = self.transaction(tx_id).unwrap().ccy;
        if self.window_open(ccy, at) {
            self.convert(at, tx_id);
        } else {
            self.queued.push(tx_id.to_string());
        }
    }

    /// Fund the reserve and mint, in one journal: the reserve rule is checked on the result.
    fn convert(&mut self, at: Ts, tx_id: &str) {
        let i = self.tx_index[tx_id];
        let t = self.state.transactions[i].clone();
        let ar = self.platform.autoramp(t.autoramp_id.as_deref().unwrap()).cloned().unwrap();
        let (c, a) = (t.ccy, t.amount_minor);
        self.post(
            at,
            "mint",
            tx_id,
            vec![dr(acct::held(&t.customer_id, c), c, a), cr(acct::tokens_pending(c), c, a), dr(acct::reserve(c), c, a), cr(acct::operating(c), c, a)],
        );
        self.sweep_refs.insert(tx_id.to_string());
        self.outbox.push(Effect::ReserveSweep { sweep_id: tx_id.to_string(), ccy: c, amount_minor: a });
        let token = self.cfg.token(c).clone();
        let mint_id = format!("mint-{tx_id}");
        let amount_base = minor_to_base_units(a, c, token.decimals);
        self.state.transactions[i].mint =
            Some(MintOp { id: mint_id.clone(), chain: ar.chain.clone(), to: ar.wallet.clone(), amount_base, submitted_at: at, attempts: 1, tx_hash: None });
        self.state.transactions[i].converted_at = Some(at);
        self.mint_index.insert(mint_id.clone(), i);
        self.outbox.push(Effect::Mint { mint_id, chain: ar.chain, token: token.symbol, to: ar.wallet, amount_base });
        self.set_status(at, tx_id, Status::PayoutInProgress, "mint submitted");
    }

    fn on_decision(&mut self, at: Ts, hold_id: &str, decision: Decision, by: &str) -> Result<()> {
        let hi = *self.hold_index.get(hold_id).ok_or_else(|| anyhow!("unknown hold {hold_id}"))?;
        if let Some((_, d, _)) = &self.state.holds[hi].decided {
            // Idempotent: the same decision again is a no-op; a different one is refused.
            return if *d == decision { Ok(()) } else { Err(anyhow!("hold {hold_id} already decided as {d:?}")) };
        }
        self.state.holds[hi].decided = Some((at, decision, by.to_string()));
        let tx_id = self.state.holds[hi].transaction_id.clone();
        let t = self.transaction(&tx_id).cloned().unwrap();
        match (t.kind, decision) {
            (TxKind::Mint, Decision::Release) => {
                self.set_status(at, &tx_id, Status::ConversionInProgress, &format!("released by {by}"));
                self.convert_or_queue(at, &tx_id);
            }
            (TxKind::Mint, Decision::Reject) => {
                self.set_status(at, &tx_id, Status::RejectedAml, &format!("rejected by {by}"));
                self.post(
                    at,
                    "blocked",
                    &tx_id,
                    vec![dr(acct::held(&t.customer_id, t.ccy), t.ccy, t.amount_minor), cr(acct::blocked(t.ccy), t.ccy, t.amount_minor)],
                );
            }
            (TxKind::Mint, Decision::Return) => {
                self.set_status(at, &tx_id, Status::Returned, &format!("returned by {by}"));
                let di = self.state.deposits.iter().position(|d| d.transaction_id.as_deref() == Some(tx_id.as_str())).unwrap();
                self.post(
                    at,
                    "held_return",
                    &tx_id,
                    vec![dr(acct::held(&t.customer_id, t.ccy), t.ccy, t.amount_minor), cr(acct::returns_due(t.ccy), t.ccy, t.amount_minor)],
                );
                self.instruct_return(at, di, "returned after review");
            }
            (TxKind::Redeem, Decision::Release) => {
                self.set_status(at, &tx_id, Status::ConversionInProgress, &format!("released by {by}"));
                self.redeem(at, &tx_id);
            }
            (TxKind::Redeem, _) => {
                // Tokens stay where they are (outstanding); a person handles the counterparty.
                self.set_status(at, &tx_id, Status::RejectedAml, &format!("rejected by {by}"));
            }
        }
        Ok(())
    }

    fn on_tick(&mut self, at: Ts) {
        // Conversions that waited for the USD window.
        let queued = std::mem::take(&mut self.queued);
        for tx in queued {
            let ccy = self.transaction(&tx).unwrap().ccy;
            if self.window_open(ccy, at) {
                self.convert(at, &tx);
            } else {
                self.queued.push(tx);
            }
        }
        // Mints that never made it into the canonical chain (dropped by a reorg, stuck in a mempool).
        let pending: Vec<(usize, String, String)> = self
            .state
            .transactions
            .iter()
            .enumerate()
            .filter(|(_, t)| t.status == Status::PayoutInProgress && t.kind == TxKind::Mint)
            .filter_map(|(i, t)| t.mint.as_ref().map(|m| (i, m.id.clone(), m.chain.clone())))
            .collect();
        for (i, mint_id, chain) in pending {
            let Some(ccfg) = self.cfg.chain(&chain) else { continue };
            let m = self.state.transactions[i].mint.clone().unwrap();
            if at - m.submitted_at < ccfg.resubmit_after_secs {
                continue;
            }
            let in_chain = self.indexers.get(&chain).map(|ix| ix.unconfirmed_ops().contains(&mint_id)).unwrap_or(false);
            if in_chain {
                continue;
            }
            let token = self.cfg.token(self.state.transactions[i].ccy).symbol.clone();
            let mm = self.state.transactions[i].mint.as_mut().unwrap();
            mm.attempts += 1;
            mm.submitted_at = at;
            self.outbox.push(Effect::Mint { mint_id, chain, token, to: m.to, amount_base: m.amount_base });
        }
    }

    fn on_block(&mut self, at: Ts, b: crate::chain::Block) -> Result<()> {
        let chain = b.chain.clone();
        let ix = self.indexers.get_mut(&chain).ok_or_else(|| anyhow!("unknown chain {chain}"))?;
        let logs = ix.push(b).map_err(|e| anyhow!(e))?;
        for l in logs {
            self.on_final_log(at, &chain, l);
        }
        Ok(())
    }

    fn token_ccy(&self, symbol: &str) -> Option<Ccy> {
        self.cfg.tokens.iter().find(|t| t.symbol == symbol).map(|t| t.ccy)
    }

    fn on_final_log(&mut self, at: Ts, chain: &str, l: Log) {
        let Some(ccy) = self.token_ccy(&l.token) else { return };
        let decimals = self.cfg.token(ccy).decimals;
        match l.kind {
            LogKind::Mint => {
                *self.state.confirmed_supply.entry(l.token.clone()).or_default() += l.amount_base;
                let op = l.op_id.clone().unwrap_or_default();
                self.state.final_ops.insert(op.clone(), (chain.to_string(), l.tx_hash.clone(), "mint".into(), l.amount_base));
                if let Some(&i) = self.mint_index.get(&op) {
                    let t = &self.state.transactions[i];
                    if t.status == Status::PayoutInProgress {
                        let (c, a, id) = (t.ccy, t.amount_minor, t.id.clone());
                        self.post(at, "mint_final", &id, vec![dr(acct::tokens_pending(c), c, a), cr(acct::tokens_outstanding(c), c, a)]);
                        self.state.transactions[i].mint.as_mut().unwrap().tx_hash = Some(l.tx_hash.clone());
                        self.set_status(at, &id, Status::Completed, "mint final on-chain");
                    }
                }
            }
            LogKind::Burn => {
                *self.state.confirmed_supply.entry(l.token.clone()).or_default() -= l.amount_base;
                *self.state.pending_burns.entry(l.token.clone()).or_default() -= l.amount_base;
                let op = l.op_id.clone().unwrap_or_default();
                self.state.final_ops.insert(op, (chain.to_string(), l.tx_hash.clone(), "burn".into(), l.amount_base));
            }
            LogKind::Transfer => {
                let redemption = self.platform.redemption_addresses.iter().any(|(c, a)| c == chain && a.eq_ignore_ascii_case(&l.to));
                if !redemption {
                    return;
                }
                let Some(amount) = crate::money::base_units_to_minor(l.amount_base, ccy, decimals) else {
                    // Sub-cent dust cannot be paid out in fiat: left on-chain, not redeemed.
                    return;
                };
                let owner = self
                    .platform
                    .customers
                    .iter()
                    .find(|c| c.wallets.iter().any(|(ch, w)| ch == chain && w.eq_ignore_ascii_case(&l.from)))
                    .map(|c| c.id.clone());
                let tx_id = self.next("tx");
                let customer_id = owner.clone().unwrap_or_else(|| "unknown".into());
                self.tx_index.insert(tx_id.clone(), self.state.transactions.len());
                self.state.transactions.push(Transaction {
                    id: tx_id.clone(),
                    kind: TxKind::Redeem,
                    deposit_id: None,
                    autoramp_id: None,
                    customer_id,
                    ccy,
                    amount_minor: amount,
                    status: Status::FundsReviewInProgress,
                    history: vec![(at, Status::FundsReviewInProgress, format!("{} tokens received from {}", l.token, l.from))],
                    hold_id: None,
                    mint: None,
                    payout_ref: None,
                    burn_id: None,
                    converted_at: None,
                });
                if owner.is_none() {
                    self.open_hold(at, &tx_id, vec![HoldReason::UnregisteredWallet { wallet: l.from.clone() }]);
                } else {
                    self.set_status(at, &tx_id, Status::ConversionInProgress, "redemption from a registered wallet");
                    self.redeem(at, &tx_id);
                }
            }
        }
    }

    /// Burn the received tokens and pay the customer out of the reserve.
    fn redeem(&mut self, at: Ts, tx_id: &str) {
        let i = self.tx_index[tx_id];
        let t = self.state.transactions[i].clone();
        let (c, a) = (t.ccy, t.amount_minor);
        let Some(cust) = self.platform.customer(&t.customer_id).cloned() else { return };
        let token = self.cfg.token(c).clone();
        let burn_id = format!("burn-{tx_id}");
        let payout_ref = format!("po-{}", &tx_id[3..]);
        self.post(at, "redeem", tx_id, vec![dr(acct::tokens_outstanding(c), c, a), cr(acct::reserve(c), c, a)]);
        let chain = cust.wallets.first().map(|(ch, _)| ch.clone()).unwrap_or_else(|| "ethereum".into());
        let burn_base = minor_to_base_units(a, c, token.decimals);
        *self.state.pending_burns.entry(token.symbol.clone()).or_default() += burn_base;
        self.outbox.push(Effect::Burn { burn_id: burn_id.clone(), chain, token: token.symbol, amount_base: burn_base });
        let (rail, to_account) = cust.payout_account.clone();
        self.outbox.push(Effect::Payout { payout_ref: payout_ref.clone(), transaction_id: tx_id.to_string(), rail, ccy: c, amount_minor: a, to_account });
        let t = &mut self.state.transactions[i];
        t.burn_id = Some(burn_id);
        t.payout_ref = Some(payout_ref.clone());
        t.converted_at = Some(at);
        self.payout_index.insert(payout_ref, i);
        self.set_status(at, tx_id, Status::PayoutInProgress, "burn and payout instructed");
    }

    fn on_payout_return(&mut self, at: Ts, r: PayoutReturn) {
        let key = r.original_ref.to_lowercase();
        let Some(&i) = self.payout_index.get(&key) else {
            self.state.payout_returns.push(r);
            return;
        };
        let t = self.state.transactions[i].clone();
        if t.status == Status::Failed {
            return; // already processed (re-sent return file)
        }
        if t.status == Status::PayoutInProgress {
            self.set_status(at, &t.id, Status::Completed, "payout executed");
        }
        self.post(
            at,
            "payout_returned",
            &t.id,
            vec![dr(acct::reserve(t.ccy), t.ccy, t.amount_minor), cr(acct::held(&t.customer_id, t.ccy), t.ccy, t.amount_minor)],
        );
        self.set_status(at, &t.id, Status::Failed, &format!("payout returned by the recipient's bank ({})", r.reason_code));
        self.state.payout_returns.push(r);
    }

    // -----------------------------------------------------------------------------------------
    // Reconciliation

    fn ledger_balance_at(&self, account: &str, ccy: Ccy, cutoff: Ts) -> i64 {
        self.state
            .ledger
            .journals
            .iter()
            .filter(|j| j.at <= cutoff)
            .flat_map(|j| j.postings.iter())
            .filter(|p| p.account == account && p.ccy == ccy)
            .map(|p| p.amount_minor)
            .sum()
    }

    fn reconcile(&mut self, at: Ts, stmts: Vec<Statement>) {
        let date = stmts.first().map(|s| crate::time::fmt_date(s.date)).unwrap_or_default();
        let mut breaks = vec![];
        let mut matched = 0usize;
        let n_statements = stmts.len();
        for s in &stmts {
            let Some((ccy, which)) = self.platform.bank_accounts.iter().find_map(|(c, o, r)| {
                if *o == s.account {
                    Some((*c, "operating"))
                } else if *r == s.account {
                    Some((*c, "reserve"))
                } else {
                    None
                }
            }) else {
                breaks.push(Break {
                    kind: BreakKind::UnknownStatementEntry,
                    account: s.account.clone(),
                    ccy: s.ccy,
                    amount_minor: s.closing_minor,
                    reference: "statement".into(),
                    explanation: "statement for an account we do not hold".into(),
                    resolved: false,
                });
                continue;
            };
            let ledger_account = if which == "operating" { acct::operating(ccy) } else { acct::reserve(ccy) };
            for e in &s.entries {
                let ok = match &e.kind {
                    EntryKind::Credit => {
                        let p = e.credit.clone().expect("credit entry carries its payment");
                        if self.by_bank_ref.contains_key(&(p.rail, p.bank_ref.clone())) {
                            true
                        } else if self.cfg.ingest_statement_only_credits {
                            breaks.push(Break {
                                kind: BreakKind::MissingNotification,
                                account: s.account.clone(),
                                ccy,
                                amount_minor: e.amount_minor,
                                reference: p.bank_ref.clone(),
                                explanation: format!(
                                    "{} credit from {} on the statement but never notified; booked as a deposit",
                                    p.rail.name(),
                                    p.debtor_name
                                ),
                                resolved: true,
                            });
                            self.on_credit(at, p, DepositSource::StatementOnly);
                            // The deposit journal is dated at booking, inside this statement's day.
                            false
                        } else {
                            breaks.push(Break {
                                kind: BreakKind::MissingNotification,
                                account: s.account.clone(),
                                ccy,
                                amount_minor: e.amount_minor,
                                reference: p.bank_ref.clone(),
                                explanation: "credit on the statement without a matching deposit".into(),
                                resolved: false,
                            });
                            self.state.open_items.entry(s.account.clone()).or_default().insert(e.bank_ref.clone(), e.amount_minor);
                            false
                        }
                    }
                    EntryKind::Fee => {
                        breaks.push(Break {
                            kind: BreakKind::BankFeeNotBooked,
                            account: s.account.clone(),
                            ccy,
                            amount_minor: e.amount_minor,
                            reference: e.bank_ref.clone(),
                            explanation: "bank charge not in the ledger; book it to fees".into(),
                            resolved: false,
                        });
                        self.state.open_items.entry(s.account.clone()).or_default().insert(e.bank_ref.clone(), e.amount_minor);
                        false
                    }
                    EntryKind::Transfer => e.our_ref.as_ref().is_some_and(|r| self.sweep_refs.contains(r)),
                    EntryKind::Outgoing => {
                        let r = e.our_ref.clone().unwrap_or_default();
                        if let Some(&ti) = self.payout_index.get(&r.to_lowercase()) {
                            let id = self.state.transactions[ti].id.clone();
                            if self.state.transactions[ti].status == Status::PayoutInProgress {
                                self.set_status(at, &id, Status::Completed, "payout on the bank statement");
                            }
                            true
                        } else {
                            self.return_index.contains_key(&r)
                        }
                    }
                    EntryKind::ReturnReceived => self.state.payout_returns.iter().any(|r| r.bank_ref == e.bank_ref),
                    EntryKind::Other(_) => false,
                };
                if ok {
                    matched += 1;
                } else if !matches!(e.kind, EntryKind::Credit | EntryKind::Fee) {
                    breaks.push(Break {
                        kind: BreakKind::UnknownStatementEntry,
                        account: s.account.clone(),
                        ccy,
                        amount_minor: e.amount_minor,
                        reference: e.bank_ref.clone(),
                        explanation: format!("{:?} entry we cannot match", e.kind),
                        resolved: false,
                    });
                    self.state.open_items.entry(s.account.clone()).or_default().insert(e.bank_ref.clone(), e.amount_minor);
                }
            }
            // Items found today and on earlier days that nobody has booked yet explain the difference.
            let explained: i64 = self.state.open_items.get(&s.account).map(|m| m.values().sum()).unwrap_or(0);
            let ledger = self.ledger_balance_at(&ledger_account, ccy, s.date);
            let diff = s.closing_minor - ledger - explained;
            if diff != 0 {
                breaks.push(Break {
                    kind: BreakKind::UnexplainedBalance,
                    account: s.account.clone(),
                    ccy,
                    amount_minor: diff,
                    reference: date.clone(),
                    explanation: format!("statement closes at {} but the ledger says {} and open items total {}", s.closing_minor, ledger, explained),
                    resolved: false,
                });
            }
        }
        // Chain and reserve: the ledger's tokens against the chain, and the bank's reserve against both.
        let mut coverage = vec![];
        for t in self.cfg.tokens.clone() {
            // Tokens already redeemed but whose burn is not final are still on-chain: count them out.
            let supply_base = self.state.confirmed_supply.get(&t.symbol).copied().unwrap_or(0) - self.state.pending_burns.get(&t.symbol).copied().unwrap_or(0);
            let supply = crate::money::base_units_to_minor(supply_base, t.ccy, t.decimals).unwrap_or(i64::MAX);
            let outstanding = -self.state.ledger.balance(&acct::tokens_outstanding(t.ccy), t.ccy);
            if supply != outstanding {
                breaks.push(Break {
                    kind: BreakKind::ChainSupplyMismatch,
                    account: acct::tokens_outstanding(t.ccy),
                    ccy: t.ccy,
                    amount_minor: supply - outstanding,
                    reference: t.symbol.clone(),
                    explanation: format!("final on-chain supply net of pending burns {supply} vs ledger outstanding {outstanding}"),
                    resolved: false,
                });
            }
            // The bank's reserve at the statement cut-off must cover every token the ledger says
            // existed or was being minted at that moment.
            let Some(stmt) = stmts.iter().find(|s| Some(s.account.as_str()) == self.platform.reserve_account(t.ccy)) else { continue };
            let cutoff = stmt.date;
            let tokens_at = -(self.ledger_balance_at(&acct::tokens_outstanding(t.ccy), t.ccy, cutoff)
                + self.ledger_balance_at(&acct::tokens_pending(t.ccy), t.ccy, cutoff));
            let pending_at = -self.ledger_balance_at(&acct::tokens_pending(t.ccy), t.ccy, cutoff);
            coverage.push((t.ccy, stmt.closing_minor, tokens_at - pending_at, pending_at));
            if stmt.closing_minor < tokens_at {
                breaks.push(Break {
                    kind: BreakKind::ReserveCoverage,
                    account: acct::reserve(t.ccy),
                    ccy: t.ccy,
                    amount_minor: stmt.closing_minor - tokens_at,
                    reference: t.symbol.clone(),
                    explanation: "bank reserve balance below the tokens outstanding and being minted at the cut-off".into(),
                    resolved: false,
                });
            }
        }
        for t in &self.state.transactions {
            if let (Status::PayoutInProgress, Some(m)) = (t.status, &t.mint) {
                if m.attempts > 1 {
                    breaks.push(Break {
                        kind: BreakKind::MintStuck,
                        account: m.chain.clone(),
                        ccy: t.ccy,
                        amount_minor: t.amount_minor,
                        reference: m.id.clone(),
                        explanation: format!("mint re-submitted {} times and not final yet", m.attempts - 1),
                        resolved: false,
                    });
                }
            }
        }
        self.state.recon.push(ReconReport { date, statements: n_statements, entries_matched: matched, breaks, coverage });
    }
}
