//! Runs the engine against a simulated world: a partner bank that books credits and sends files, two
//! chains that make blocks (and occasionally reorganise), customers who redeem, and an operations desk
//! that decides held deposits the way a person with the full facts would.

use super::truth::WorldTruth;
use super::world::{Notify, PlannedPayment, Rng, World};
use crate::chain::{Block, Log, LogKind};
use crate::config::EngineConfig;
use crate::domain::{InboundPayment, PayoutReturn, Rail};
use crate::engine::{Decision, Effect, Engine, Input, TxKind};
use crate::money::Ccy;
use crate::rails::iso::{EntryKind, Statement, StatementEntry, write_camt053, write_camt054, write_pacs004, write_pacs008};
use crate::rails::{fedwire, nacha};
use crate::time::{DAY, HOUR, MINUTE, Ts, fmt_date, weekday};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};

#[derive(Clone, Debug)]
enum Ev {
    File { name: String, raw: String },
    Statement { day: i64 },
    Block { chain: String },
    Decide { hold_id: String, decision: Decision },
    Tick,
    PayoutReturn { payout_ref: String, rail: Rail, ccy: Ccy, amount: i64 },
    Redeem { unregistered: bool },
}

#[derive(Default)]
struct Queue {
    heap: BinaryHeap<Reverse<(Ts, u64)>>,
    evs: HashMap<u64, Ev>,
    seq: u64,
}

impl Queue {
    fn push(&mut self, t: Ts, e: Ev) {
        self.seq += 1;
        self.heap.push(Reverse((t, self.seq)));
        self.evs.insert(self.seq, e);
    }
    fn pop(&mut self) -> Option<(Ts, Ev)> {
        let Reverse((t, s)) = self.heap.pop()?;
        Some((t, self.evs.remove(&s).unwrap()))
    }
}

struct BankAcct {
    ccy: Ccy,
    entries: Vec<StatementEntry>,
}

struct Bank {
    accts: BTreeMap<String, BankAcct>,
    closing: BTreeMap<String, i64>,
    seq: u64,
}

impl Bank {
    fn book(&mut self, account: &str, e: StatementEntry) {
        self.accts.get_mut(account).expect("bank account").entries.push(e);
    }
    fn next_ref(&mut self, p: &str) -> String {
        self.seq += 1;
        format!("{p}{:010}", self.seq)
    }
    fn statement(&mut self, account: &str, day: i64) -> Statement {
        let a = &self.accts[account];
        let (from, to) = (day * DAY, day * DAY + DAY);
        let mut entries: Vec<StatementEntry> = a.entries.iter().filter(|e| e.booked_at >= from && e.booked_at < to).cloned().collect();
        entries.sort_by_key(|e| e.booked_at);
        let opening = *self.closing.get(account).unwrap_or(&0);
        let closing = opening + entries.iter().map(|e| e.amount_minor).sum::<i64>();
        self.closing.insert(account.to_string(), closing);
        Statement { account: account.to_string(), ccy: a.ccy, date: to - 1, opening_minor: opening, closing_minor: closing, entries }
    }
}

#[derive(Clone)]
struct SimBlock {
    hash: String,
    logs: Vec<Log>,
}

struct ChainSim {
    name: String,
    blocks: Vec<SimBlock>,
    mempool: Vec<Log>,
    canonical_ops: HashSet<String>,
    /// op → drop (false) or re-include (true), decided when first submitted.
    plans: HashMap<String, bool>,
    /// (height of the block holding the op, op, re-include?, blocks to wait)
    fork_due: Option<(usize, String, bool, u32)>,
    balances: HashMap<(String, String), i128>, // (wallet, token) → base units
    announced: bool,
}

impl ChainSim {
    fn new(name: &str, rng: &mut Rng) -> Self {
        let g = SimBlock { hash: format!("{name}-genesis-{:x}", rng.u64()), logs: vec![] };
        ChainSim {
            name: name.into(),
            blocks: vec![g],
            mempool: vec![],
            canonical_ops: HashSet::new(),
            plans: HashMap::new(),
            fork_due: None,
            balances: HashMap::new(),
            announced: false,
        }
    }

    fn block_msg(&self, n: usize, time: Ts) -> Block {
        Block {
            chain: self.name.clone(),
            number: n as u64,
            hash: self.blocks[n].hash.clone(),
            parent: if n == 0 { String::new() } else { self.blocks[n - 1].hash.clone() },
            time,
            logs: self.blocks[n].logs.clone(),
        }
    }

    fn apply_balances(&mut self, logs: &[Log], sign: i128) {
        for l in logs {
            match l.kind {
                LogKind::Mint => *self.balances.entry((l.to.clone(), l.token.clone())).or_default() += sign * l.amount_base,
                LogKind::Burn => {}
                LogKind::Transfer => {
                    *self.balances.entry((l.from.clone(), l.token.clone())).or_default() -= sign * l.amount_base;
                    *self.balances.entry((l.to.clone(), l.token.clone())).or_default() += sign * l.amount_base;
                }
            }
        }
    }

    /// Make the next block; may also emit a competing fork. Returns blocks to deliver, in order.
    fn produce(&mut self, rng: &mut Rng, time: Ts, truth: &mut WorldTruth) -> Vec<Block> {
        let mut logs = vec![];
        for l in std::mem::take(&mut self.mempool) {
            if let Some(op) = &l.op_id {
                if self.canonical_ops.contains(op) {
                    continue; // the issuer contract refuses an op ID it has already executed
                }
                self.canonical_ops.insert(op.clone());
            }
            logs.push(l);
        }
        let n = self.blocks.len();
        if self.fork_due.is_none() {
            if let Some(l) = logs.iter().find(|l| l.op_id.as_ref().is_some_and(|o| self.plans.contains_key(o))) {
                let op = l.op_id.clone().unwrap();
                let re = self.plans.remove(&op).unwrap();
                self.fork_due = Some((n, op, re, 2));
            }
        }
        self.apply_balances(&logs, 1);
        self.blocks.push(SimBlock { hash: format!("0x{:016x}{:016x}", rng.u64(), rng.u64()), logs });
        let mut out = vec![];
        if !self.announced {
            self.announced = true;
            out.push(self.block_msg(0, time));
        }
        out.push(self.block_msg(n, time));
        if let Some((h, op, re, wait)) = self.fork_due.clone() {
            if wait > 0 {
                self.fork_due = Some((h, op, re, wait - 1));
            } else {
                self.fork_due = None;
                // Replace blocks h.. with a longer branch that leaves the op out (or puts it back later).
                let old: Vec<SimBlock> = self.blocks.drain(h..).collect();
                for b in &old {
                    self.apply_balances(&b.logs, -1);
                }
                let mut carried: Vec<Log> = old.iter().flat_map(|b| b.logs.clone()).collect();
                let target = carried.iter().position(|l| l.op_id.as_deref() == Some(op.as_str())).unwrap();
                let target_log = carried.remove(target);
                let replaced = old.len() + 1;
                for k in 0..replaced {
                    let mut logs = if k == 0 { std::mem::take(&mut carried) } else { vec![] };
                    if re && k == 1 {
                        logs.push(target_log.clone());
                    }
                    self.apply_balances(&logs, 1);
                    self.blocks.push(SimBlock { hash: format!("0x{:016x}{:016x}", rng.u64(), rng.u64()), logs });
                    out.push(self.block_msg(h + k, time));
                }
                if re {
                    truth.reorg_reincluded_mints += 1;
                } else {
                    self.canonical_ops.remove(&op);
                    truth.reorg_dropped_mints += 1;
                }
            }
        }
        out
    }
}

pub struct RunStats {
    pub inputs: u64,
    pub files: u64,
    pub blocks: u64,
    pub ticks: u64,
    pub webhooks: u64,
    pub wall_ms: u128,
    pub engine_ms: u128,
}

pub struct RunOutput {
    pub engine: Engine,
    pub truth: WorldTruth,
    pub stats: RunStats,
    /// Every input in order, when asked for (to replay into Postgres).
    pub inputs: Vec<(Ts, Input)>,
}

fn next_business(t: Ts) -> Ts {
    let mut d = t.div_euclid(DAY);
    while weekday(d) >= 5 {
        d += 1;
    }
    d * DAY + t.rem_euclid(DAY).max(8 * HOUR)
}

pub fn run(world: &World, cfg: EngineConfig, keep_inputs: bool) -> RunOutput {
    let wall = std::time::Instant::now();
    let wc = &world.cfg;
    let mut rng = Rng::new(wc.seed.wrapping_mul(31).wrapping_add(7));
    let mut engine = Engine::new(cfg, world.platform.clone());
    let mut truth = WorldTruth { deposits: world.payments.iter().map(|p| p.truth.clone()).collect(), ..Default::default() };
    let truth_by_ref: HashMap<String, usize> = truth.deposits.iter().enumerate().map(|(i, d)| (d.bank_ref.clone(), i)).collect();
    let pl = &world.platform;
    let mut bank = Bank { accts: BTreeMap::new(), closing: BTreeMap::new(), seq: 0 };
    for (ccy, op, res) in &pl.bank_accounts {
        bank.accts.insert(op.clone(), BankAcct { ccy: *ccy, entries: vec![] });
        bank.accts.insert(res.clone(), BankAcct { ccy: *ccy, entries: vec![] });
    }
    let mut chains: BTreeMap<String, ChainSim> = BTreeMap::new();
    for c in ["ethereum", "solana"] {
        chains.insert(c.to_string(), ChainSim::new(c, &mut rng));
    }
    let mut q = Queue::default();
    let start = super::world::first_day(wc) * DAY;
    let end = super::world::last_day(wc) * DAY + 5 * DAY;

    // ---- the bank books every credit and decides when to tell us
    let mut payments: Vec<PlannedPayment> = world.payments.clone();
    // ACH credits settle in three windows a day and are booked at the window.
    for p in payments.iter_mut().filter(|p| matches!(p.notify, Notify::AchFile)) {
        let day = p.payment.booked_at.div_euclid(DAY);
        let tod = p.payment.booked_at.rem_euclid(DAY);
        let w = [13 * HOUR, 17 * HOUR, 21 * HOUR].into_iter().find(|w| *w >= tod).unwrap_or(21 * HOUR);
        p.payment.booked_at = day * DAY + w;
    }
    let mut camt_batches: BTreeMap<Ts, Vec<InboundPayment>> = BTreeMap::new();
    let mut ach_batches: BTreeMap<Ts, Vec<InboundPayment>> = BTreeMap::new();
    for p in &payments {
        let pay = &p.payment;
        let op = pl.operating_account(pay.ccy).unwrap().to_string();
        bank.book(
            &op,
            StatementEntry {
                bank_ref: pay.bank_ref.clone(),
                amount_minor: pay.amount_minor,
                kind: EntryKind::Credit,
                booked_at: pay.booked_at,
                our_ref: None,
                credit: Some(pay.clone()),
            },
        );
        match p.notify {
            Notify::Camt054 => {
                let file_t = (pay.booked_at.div_euclid(HOUR) + 1) * HOUR + 5 * MINUTE;
                camt_batches.entry(file_t).or_default().push(pay.clone());
                if p.renotify {
                    camt_batches.entry(file_t + HOUR).or_default().push(pay.clone());
                    truth.renotified += 1;
                }
            }
            Notify::Pacs008 => {
                let t = pay.booked_at + rng.range(2, 10) * MINUTE;
                let raw = write_pacs008(&format!("M{}", rng.alnum(10)), pay.booked_at, pay, "DEUTDEFFXXX");
                q.push(t, Ev::File { name: format!("pacs008-{}.xml", pay.bank_ref), raw: raw.clone() });
                if p.renotify {
                    q.push(t + 30 * MINUTE, Ev::File { name: format!("pacs008-{}-resent.xml", pay.bank_ref), raw });
                    truth.renotified += 1;
                }
            }
            Notify::AchFile => ach_batches.entry(pay.booked_at).or_default().push(pay.clone()),
            Notify::Fedwire => {
                q.push(pay.booked_at + MINUTE, Ev::File { name: format!("fedwire-{}.txt", pay.bank_ref), raw: fedwire::write(pay, "026009593") })
            }
            Notify::StatementOnly => truth.statement_only += 1,
        }
    }
    let mut camt_corrupt = wc.corrupt_files / 2;
    let mut ach_corrupt = wc.corrupt_files - camt_corrupt;
    for (t, credits) in &camt_batches {
        let acct = pl.operating_account(Ccy::EUR).unwrap();
        let raw = write_camt054(&format!("N{}", rng.alnum(10)), *t, acct, Ccy::EUR, credits);
        let name = format!("camt054-{}.xml", crate::time::fmt_iso(*t).replace(':', ""));
        if camt_corrupt > 0 && credits.len() >= 3 && *t > start + 3 * DAY {
            camt_corrupt -= 1;
            truth.corrupt_files.push(name.clone());
            let mut cut = raw.len() / 2;
            while !raw.is_char_boundary(cut) {
                cut -= 1;
            }
            q.push(*t, Ev::File { name: name.clone(), raw: raw[..cut].to_string() });
            q.push(*t + HOUR, Ev::File { name: format!("{name}.resent"), raw });
        } else {
            q.push(*t, Ev::File { name, raw });
        }
    }
    for (t, credits) in &ach_batches {
        let entries: Vec<nacha::AchCredit> = credits
            .iter()
            .map(|p| nacha::AchCredit {
                company_name: p.debtor_name.clone(),
                company_id: p.debtor_account.chars().take(10).collect(),
                sec: "CCD".into(),
                entry_description: "PAYMENT".into(),
                rdfi_routing: world.routing_ours.clone(),
                account: p.creditor_account.clone(),
                amount_cents: p.amount_minor,
                individual_id: p.structured_ref.clone().unwrap_or_default(),
                individual_name: "MOONPAY ENTERPRISE".into(),
                trace: p.bank_ref.clone(),
                addenda: p.unstructured.clone(),
                return_of: None,
            })
            .collect();
        let raw = nacha::write_file(*t, "ORIGIN BANK", &entries);
        let name = format!("ach-{}.txt", crate::time::fmt_iso(*t).replace(':', ""));
        if ach_corrupt > 0 && *t > start + 5 * DAY {
            ach_corrupt -= 1;
            truth.corrupt_files.push(name.clone());
            let truncated: String = raw.lines().filter(|l| !l.starts_with('9')).map(|l| format!("{l}\n")).collect();
            q.push(*t, Ev::File { name: name.clone(), raw: truncated });
            q.push(*t + 2 * HOUR, Ev::File { name: format!("{name}.resent"), raw });
        } else {
            q.push(*t, Ev::File { name, raw });
        }
    }
    // Bank charges, visible only on the statement.
    for day in start.div_euclid(DAY)..end.div_euclid(DAY) - 2 {
        if weekday(day) >= 5 {
            continue;
        }
        for (ccy, op, _) in &pl.bank_accounts {
            if rng.chance(wc.p_fee_per_account_day) {
                let v = rng.range(250, 4500);
                let r = bank.next_ref("FEE");
                bank.book(
                    op,
                    StatementEntry { bank_ref: r, amount_minor: -v, kind: EntryKind::Fee, booked_at: day * DAY + 18 * HOUR, our_ref: None, credit: None },
                );
                truth.fees += 1;
                truth.fee_total_minor += v;
                let _ = ccy;
            }
        }
    }
    // Clocks.
    let mut t = start;
    while t < end {
        q.push(t, Ev::Tick);
        t += 15 * MINUTE;
    }
    for d in start.div_euclid(DAY)..end.div_euclid(DAY) {
        q.push((d + 1) * DAY + 90 * MINUTE, Ev::Statement { day: d });
    }
    for c in chains.keys() {
        let mut t = start;
        while t < end {
            q.push(t, Ev::Block { chain: c.clone() });
            t += wc.block_secs;
        }
    }
    for d in (start.div_euclid(DAY) + 2)..(end.div_euclid(DAY) - 4) {
        if weekday(d) >= 5 {
            continue;
        }
        for _ in 0..wc.redemptions_per_day {
            q.push(d * DAY + rng.range(8 * HOUR, 20 * HOUR), Ev::Redeem { unregistered: false });
        }
    }
    let span = (end - start - 6 * DAY).max(DAY);
    for _ in 0..wc.unregistered_wallet_redemptions {
        q.push(start + 3 * DAY + rng.range(0, span), Ev::Redeem { unregistered: true });
    }

    // ---- reorg plans: decided on mint submission
    let mut drops_left = wc.reorg_drops;
    let mut reincl_left = wc.reorg_reincludes;
    let mut returns_left = wc.payout_returns;
    let mut stats = RunStats { inputs: 0, files: 0, blocks: 0, ticks: 0, webhooks: 0, wall_ms: 0, engine_ms: 0 };
    let mut inputs: Vec<(Ts, Input)> = vec![];
    let mut engine_time = std::time::Duration::ZERO;

    while let Some((t, ev)) = q.pop() {
        let mut to_engine: Vec<Input> = vec![];
        match ev {
            Ev::File { name, raw } => {
                stats.files += 1;
                to_engine.push(Input::BankFile { name, raw });
            }
            Ev::Statement { day } => {
                let accounts: Vec<String> = bank.accts.keys().cloned().collect();
                let stmts: Vec<Statement> = accounts.iter().map(|a| bank.statement(a, day)).collect();
                let raw: String = {
                    // One camt.053 file holding every account's statement.
                    let parts: Vec<String> = stmts.iter().map(|s| write_camt053(&format!("S{}", rng.alnum(8)), s)).collect();
                    let first = &parts[0];
                    let head_end = first.find("<Stmt>").unwrap();
                    let tail_start = first.rfind("</BkToCstmrStmt>").unwrap();
                    let mut body = String::new();
                    for p in &parts {
                        body.push_str(&p[p.find("<Stmt>").unwrap()..p.rfind("</BkToCstmrStmt>").unwrap()]);
                    }
                    format!("{}{}{}", &first[..head_end], body, &first[tail_start..])
                };
                stats.files += 1;
                to_engine.push(Input::BankFile { name: format!("camt053-{}.xml", fmt_date(day * DAY)), raw });
            }
            Ev::Block { chain } => {
                let c = chains.get_mut(&chain).unwrap();
                for b in c.produce(&mut rng, t, &mut truth) {
                    stats.blocks += 1;
                    to_engine.push(Input::ChainBlock(b));
                }
            }
            Ev::Decide { hold_id, decision } => to_engine.push(Input::Decision { hold_id, decision, by: "ops-desk".into() }),
            Ev::Tick => {
                stats.ticks += 1;
                to_engine.push(Input::Tick);
            }
            Ev::PayoutReturn { payout_ref, rail, ccy, amount } => {
                let res = pl.reserve_account(ccy).unwrap().to_string();
                let rref = bank.next_ref("RTN");
                // An ACH return is known by its own trace number; a pacs.004 by its return ID.
                let rref = if rail == Rail::Ach { format!("{:0>15}", rref.trim_start_matches("RTN")) } else { rref };
                bank.book(
                    &res,
                    StatementEntry {
                        bank_ref: rref.clone(),
                        amount_minor: amount,
                        kind: EntryKind::ReturnReceived,
                        booked_at: t,
                        our_ref: Some(payout_ref.clone()),
                        credit: None,
                    },
                );
                let raw = if rail == Rail::Ach {
                    let e = nacha::AchCredit {
                        company_name: "PIE BANK NA".into(),
                        company_id: "RETURNS".into(),
                        sec: "CCD".into(),
                        entry_description: "RETURN".into(),
                        rdfi_routing: world.routing_ours.clone(),
                        account: res.clone(),
                        amount_cents: amount,
                        individual_id: String::new(),
                        individual_name: "MOONPAY ENTERPRISE".into(),
                        trace: rref.clone(),
                        addenda: None,
                        return_of: Some(("R03".into(), payout_ref.clone())),
                    };
                    nacha::write_file(t, "PIE BANK NA", &[e])
                } else {
                    write_pacs004(
                        &format!("R{}", rng.alnum(8)),
                        t,
                        &PayoutReturn {
                            rail,
                            bank_ref: rref.clone(),
                            original_ref: payout_ref.clone(),
                            amount_minor: amount,
                            ccy,
                            reason_code: "AC04".into(),
                            booked_at: t,
                        },
                    )
                };
                stats.files += 1;
                to_engine.push(Input::BankFile { name: format!("return-{payout_ref}"), raw });
                truth.payout_returns += 1;
            }
            Ev::Redeem { unregistered } => {
                // A customer with tokens sends some to the redemption address.
                let mut holders: Vec<(String, String, String, i128)> = vec![]; // chain, wallet, token, balance
                for (cname, c) in &chains {
                    for ((w, tok), bal) in &c.balances {
                        if *bal >= 1_000_000_000 && pl.customers.iter().any(|cu| cu.wallets.iter().any(|(ch, ww)| ch == cname && ww == w)) {
                            holders.push((cname.clone(), w.clone(), tok.clone(), *bal));
                        }
                    }
                }
                holders.sort();
                if holders.is_empty() {
                    continue;
                }
                let (chain, wallet, token, bal) = rng.pick(&holders).clone();
                let unit = 10_000i128; // one cent at 6 decimals
                let amount = ((bal * rng.range(10, 60) as i128 / 100) / unit) * unit;
                let redeem_to = pl.redemption_addresses.iter().find(|(c, _)| *c == chain).unwrap().1.clone();
                let c = chains.get_mut(&chain).unwrap();
                let from = if unregistered {
                    let fresh = format!("0xfresh{:x}", rng.u64());
                    c.mempool.push(Log {
                        tx_hash: format!("0x{:x}", rng.u64()),
                        kind: LogKind::Transfer,
                        token: token.clone(),
                        op_id: None,
                        from: wallet.clone(),
                        to: fresh.clone(),
                        amount_base: amount,
                    });
                    truth.unregistered_wallet_redemptions += 1;
                    fresh
                } else {
                    truth.redemptions += 1;
                    wallet.clone()
                };
                c.mempool.push(Log {
                    tx_hash: format!("0x{:x}", rng.u64()),
                    kind: LogKind::Transfer,
                    token,
                    op_id: None,
                    from,
                    to: redeem_to,
                    amount_base: amount,
                });
            }
        }
        for inp in to_engine {
            if keep_inputs {
                inputs.push((t, inp.clone()));
            }
            stats.inputs += 1;
            let t0 = std::time::Instant::now();
            engine.handle(t, inp).unwrap_or_else(|e| panic!("engine refused an input at {}: {e:#}", crate::time::fmt_iso(t)));
            engine_time += t0.elapsed();
            for eff in engine.drain() {
                match eff {
                    Effect::ReturnPayment { return_id, ccy, amount_minor, .. } => {
                        let op = pl.operating_account(ccy).unwrap().to_string();
                        let r = bank.next_ref("OUT");
                        bank.book(
                            &op,
                            StatementEntry {
                                bank_ref: r,
                                amount_minor: -amount_minor,
                                kind: EntryKind::Outgoing,
                                booked_at: t,
                                our_ref: Some(return_id),
                                credit: None,
                            },
                        );
                    }
                    Effect::ReserveSweep { sweep_id, ccy, amount_minor } => {
                        let (_, op, res) = pl.bank_accounts.iter().find(|(c, _, _)| *c == ccy).unwrap().clone();
                        let r = bank.next_ref("BOOK");
                        bank.book(
                            &op,
                            StatementEntry {
                                bank_ref: r.clone(),
                                amount_minor: -amount_minor,
                                kind: EntryKind::Transfer,
                                booked_at: t,
                                our_ref: Some(sweep_id.clone()),
                                credit: None,
                            },
                        );
                        bank.book(
                            &res,
                            StatementEntry { bank_ref: r, amount_minor, kind: EntryKind::Transfer, booked_at: t, our_ref: Some(sweep_id), credit: None },
                        );
                    }
                    Effect::Mint { mint_id, chain, token, to, amount_base } => {
                        let c = chains.get_mut(&chain).unwrap();
                        let first_time = !c.plans.contains_key(&mint_id) && !c.canonical_ops.contains(&mint_id);
                        if first_time && t > start + 2 * DAY {
                            if drops_left > 0 && rng.chance(0.01) {
                                drops_left -= 1;
                                c.plans.insert(mint_id.clone(), false);
                            } else if reincl_left > 0 && rng.chance(0.01) {
                                reincl_left -= 1;
                                c.plans.insert(mint_id.clone(), true);
                            }
                        }
                        c.mempool.push(Log {
                            tx_hash: format!("0x{:x}", rng.u64()),
                            kind: LogKind::Mint,
                            token,
                            op_id: Some(mint_id),
                            from: "0x0".into(),
                            to,
                            amount_base,
                        });
                    }
                    Effect::Burn { burn_id, chain, token, amount_base } => {
                        let redeem = pl.redemption_addresses.iter().find(|(c, _)| *c == chain).unwrap().1.clone();
                        chains.get_mut(&chain).unwrap().mempool.push(Log {
                            tx_hash: format!("0x{:x}", rng.u64()),
                            kind: LogKind::Burn,
                            token,
                            op_id: Some(burn_id),
                            from: redeem,
                            to: "0x0".into(),
                            amount_base,
                        });
                    }
                    Effect::Payout { payout_ref, rail, ccy, amount_minor, .. } => {
                        let res = pl.reserve_account(ccy).unwrap().to_string();
                        let r = bank.next_ref("PO");
                        bank.book(
                            &res,
                            StatementEntry {
                                bank_ref: r,
                                amount_minor: -amount_minor,
                                kind: EntryKind::Outgoing,
                                booked_at: t,
                                our_ref: Some(payout_ref.clone()),
                                credit: None,
                            },
                        );
                        if returns_left > 0 && rng.chance(0.06) {
                            returns_left -= 1;
                            q.push(next_business(t + rng.range(DAY, 2 * DAY)), Ev::PayoutReturn { payout_ref, rail, ccy, amount: amount_minor });
                        }
                    }
                    Effect::HoldOpened { hold_id, transaction_id, reasons } => {
                        // The ops desk looks the deposit up and decides as someone with the facts would.
                        let tx = engine.transaction(&transaction_id).cloned().unwrap();
                        let (decision, delay) = if tx.kind == TxKind::Redeem {
                            (Decision::Reject, rng.range(2 * HOUR, 20 * HOUR))
                        } else {
                            let dep = engine.state.deposits.iter().find(|d| d.transaction_id.as_deref() == Some(transaction_id.as_str())).unwrap();
                            let tr = truth_by_ref.get(&dep.payment.bank_ref).map(|&i| &truth.deposits[i]);
                            let d = match tr.and_then(|x| x.review_decision.as_deref()) {
                                Some("reject") => Decision::Reject,
                                Some("return") => Decision::Return,
                                _ => Decision::Release,
                            };
                            let edd = reasons.iter().any(|r| r == "customer_in_edd");
                            (d, if edd { rng.range(24 * HOUR, 60 * HOUR) } else { rng.range(2 * HOUR, 20 * HOUR) })
                        };
                        q.push(t + delay, Ev::Decide { hold_id, decision });
                    }
                    Effect::Webhook { .. } => stats.webhooks += 1,
                }
            }
        }
    }
    stats.wall_ms = wall.elapsed().as_millis();
    stats.engine_ms = engine_time.as_millis();
    RunOutput { engine, truth, stats, inputs }
}
