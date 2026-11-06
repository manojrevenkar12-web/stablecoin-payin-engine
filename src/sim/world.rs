//! A simulated month of MoonPay-Enterprise-style pay-ins: customers and their Autoramps, senders who
//! make the mistakes senders make, a sanctions list, and the planted cases a pay-in engine must get
//! right. Everything is generated from a seed and `config/world*.json`.

use super::truth::{DepositTruth, Expected, Kind};
use crate::domain::*;
use crate::money::Ccy;
use crate::time::{DAY, HOUR, MINUTE, Ts, days_from_civil, fmt_date, weekday};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorldCfg {
    pub name: String,
    pub seed: u64,
    pub start: String,
    pub business_days: u32,
    pub customers: usize,
    pub platform_customer_share: f64,
    pub edd_customers: usize,
    pub suspended_customers: usize,
    pub pay_by_reference_share: f64,
    pub deposits_per_day: usize,
    /// Share of EUR deposits sent by SWIFT rather than SEPA.
    pub eur_swift_share: f64,
    /// USD rails: ACH, Fedwire, SWIFT.
    pub usd_rail_mix: [f64; 3],
    pub usd_share: f64,
    /// Per-deposit probabilities of each planted case (the rest are clean).
    pub p_reformatted: f64,
    pub p_typo_customer: f64,
    pub p_typo_stranger: f64,
    pub p_typo_ambiguous: f64,
    pub p_missing_reference: f64,
    pub p_cancelled: f64,
    pub p_unknown_account: f64,
    pub p_resend: f64,
    pub p_legit_repeat: f64,
    pub p_third_party_not_allowed: f64,
    pub p_below_minimum: f64,
    pub p_renotify: f64,
    pub sanctioned_exact: usize,
    pub sanctioned_variant: usize,
    pub near_miss_innocent: usize,
    pub statement_only: usize,
    pub corrupt_files: usize,
    pub p_fee_per_account_day: f64,
    pub reorg_drops: usize,
    pub reorg_reincludes: usize,
    pub redemptions_per_day: usize,
    pub payout_returns: usize,
    pub unregistered_wallet_redemptions: usize,
    pub block_secs: i64,
}

impl WorldCfg {
    pub fn load(name: &str) -> anyhow::Result<WorldCfg> {
        let p = crate::config::repo_root().join("config").join(format!("{name}.json"));
        Ok(serde_json::from_str(&std::fs::read_to_string(&p)?)?)
    }
    pub fn start_ts(&self) -> Ts {
        crate::time::parse_iso(&self.start).expect("valid start date")
    }
}

/// SplitMix64: small, fast, and identical on every platform.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    pub fn u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn f64(&mut self) -> f64 {
        (self.u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn chance(&mut self, p: f64) -> bool {
        self.f64() < p
    }
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.u64() % ((hi - lo + 1) as u64)) as i64
    }
    pub fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[(self.u64() % v.len() as u64) as usize]
    }
    pub fn alnum(&mut self, n: usize) -> String {
        const A: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        (0..n).map(|_| A[(self.u64() % A.len() as u64) as usize] as char).collect()
    }
    pub fn digits(&mut self, n: usize) -> String {
        (0..n).map(|_| char::from(b'0' + (self.u64() % 10) as u8)).collect()
    }
    pub fn uuid(&mut self) -> String {
        let h = format!("{:016x}{:016x}", self.u64(), self.u64());
        format!("{}-{}-4{}-a{}-{}", &h[0..8], &h[8..12], &h[13..16], &h[17..20], &h[20..32])
    }
}

/// A German IBAN with correct check digits (ISO 13616 mod 97).
pub fn de_iban(blz: &str, account: &str) -> String {
    let bban = format!("{blz}{account:0>10}");
    let numeric = format!("{bban}131400"); // "DE" = 13 14, then "00"
    let mut rem: u64 = 0;
    for ch in numeric.chars() {
        rem = (rem * 10 + ch.to_digit(10).unwrap() as u64) % 97;
    }
    format!("DE{:02}{bban}", 98 - rem)
}

pub fn iban_valid(iban: &str) -> bool {
    let s: String = iban.chars().filter(|c| !c.is_whitespace()).collect();
    if s.len() < 15 {
        return false;
    }
    let rearranged = format!("{}{}", &s[4..], &s[..4]);
    let mut rem: u64 = 0;
    for ch in rearranged.chars() {
        let v = if ch.is_ascii_digit() { ch.to_digit(10).unwrap() as u64 } else { (ch.to_ascii_uppercase() as u64) - 55 };
        let digits = if v >= 10 { 100 } else { 10 };
        rem = (rem * digits + v) % 97;
    }
    rem == 1
}

const PREFIX: &[&str] = &[
    "Northwind",
    "Blue Harbor",
    "Bright Path",
    "Kestrel",
    "Alpine",
    "Lumen",
    "Cedar",
    "Harbor Point",
    "Silverline",
    "Orbit",
    "Meridian",
    "Atlas",
    "Pioneer",
    "Copper",
    "Granite",
    "Riverstone",
    "Sunfield",
    "Nova",
    "Vertex",
    "Evergreen",
    "Quarry",
    "Beacon",
    "Summit",
    "Lighthouse",
    "Ironwood",
    "Westgate",
    "Crescent Bay",
    "Falcon",
    "Juniper",
    "Mosaic",
    "Tidewater",
    "Redwood",
    "Polaris",
    "Heron",
    "Saffron",
    "Bluebird",
    "Oakridge",
    "Marlin",
    "Aster",
    "Larkspur",
    "Driftwood",
    "Fjord",
    "Solstice",
    "Kingfisher",
    "Monarch",
    "Harvest Moon",
    "Starling",
    "Wildflower",
    "Basalt",
    "Cobalt",
];
const INDUSTRY: &[&str] = &[
    "Logistics",
    "Payments",
    "Labs",
    "Software",
    "Freight",
    "Media",
    "Studios",
    "Commerce",
    "Analytics",
    "Retail",
    "Energy",
    "Health",
    "Travel",
    "Foods",
    "Robotics",
    "Markets",
    "Exports",
    "Systems",
    "Payroll",
    "Games",
];
const FORMS: &[&str] = &["GmbH", "Ltd", "LLC", "Inc", "SAS", "S.L.", "B.V.", "S.r.l.", "AG", "Limited"];
const FIRST: &[&str] = &[
    "Anna", "Lukas", "Maria", "Jonas", "Sofia", "Mateo", "Emma", "Noah", "Lea", "Elias", "Mia", "Ben", "Clara", "Paul", "Olga", "Aisha", "Chen", "Wei",
    "Priya", "Arjun", "Elena", "Marco", "Giulia", "Pierre", "Camille", "Jakub", "Zofia", "Tomas", "Ingrid", "Sven", "Amara", "Kofi", "Lucia", "Diego", "Hana",
    "Kenji", "Freya", "Oskar",
];
const LAST: &[&str] = &[
    "Schmidt",
    "Müller",
    "Garcia",
    "Rossi",
    "Dubois",
    "Nowak",
    "Kowalski",
    "Khan",
    "Patel",
    "Silva",
    "Santos",
    "Jensen",
    "Larsen",
    "Novak",
    "Horvat",
    "Yilmaz",
    "Tanaka",
    "Kim",
    "Nguyen",
    "Fischer",
    "Weber",
    "Moreau",
    "Ricci",
    "Andersson",
    "Okafor",
    "Mensah",
    "Costa",
    "Lindqvist",
    "Brennan",
    "Haddad",
    "Varga",
    "Ferreira",
];

/// Fictional listed parties. Invented for this simulation; any resemblance to a real person or
/// company is accidental.
const LISTED_PEOPLE: &[&str] = &[
    "Mohammed Tariq Al-Zahrani",
    "Sergei Viktorovich Ostrakov",
    "Yusuf Kemal Draganov",
    "Dmitry Arkadyevich Lomov",
    "Hussein Farid Qasemi",
    "Aleksandr Pavlovich Kurtin",
    "Abdullah Nasser Bakhtari",
    "Mikhail Ilyich Vorontsev",
    "Yuri Gennadyevich Saltanov",
    "Kim Chol Jun",
    "Rafael Ignacio Montebravo",
    "Zhang Weiming Holdings",
];
const LISTED_ENTITIES: &[&str] = &[
    "Volga Maritime Trading LLC",
    "Golden Crescent Exchange FZE",
    "Pyong Star Shipping Company",
    "Tabriz Petrochem Supply Co",
    "Black Sea Grain International",
    "Caspian Bridge Finance Ltd",
    "Orinoco Gold Export SA",
    "Arctic Hydra Logistics",
    "Sable Dunes General Trading",
    "Novorossa Arms Supply",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Notify {
    Camt054,
    Pacs008,
    AchFile,
    Fedwire,
    StatementOnly,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannedPayment {
    pub payment: InboundPayment,
    pub notify: Notify,
    pub renotify: bool,
    pub truth: DepositTruth,
}

#[derive(Clone, Debug)]
pub struct Payer {
    pub name: String,
    pub account: String,
}

#[derive(Clone, Debug)]
pub struct World {
    pub cfg: WorldCfg,
    pub platform: Platform,
    pub payments: Vec<PlannedPayment>,
    /// customer id → their own payer identities.
    pub customer_payers: Vec<(String, Vec<Payer>)>,
    pub routing_ours: String,
}

fn business_days(cfg: &WorldCfg) -> Vec<i64> {
    let mut d = cfg.start_ts().div_euclid(DAY);
    let mut out = vec![];
    while out.len() < cfg.business_days as usize {
        if weekday(d) <= 4 {
            out.push(d);
        }
        d += 1;
    }
    out
}

fn amount(rng: &mut Rng) -> i64 {
    // Log-uniform between 500 and 250,000 currency units; a third are round numbers.
    let units = (500f64.ln() + rng.f64() * (250_000f64.ln() - 500f64.ln())).exp();
    if rng.chance(0.33) { ((units / 1000.0).round().max(1.0) * 1000.0 * 100.0) as i64 } else { (units * 100.0).round() as i64 }
}

fn mutate_one(rng: &mut Rng, body: &str) -> String {
    let mut c: Vec<char> = body.chars().collect();
    match rng.range(0, 2) {
        0 => {
            // substitution, often a look-alike
            let i = rng.range(0, c.len() as i64 - 1) as usize;
            let look = match c[i] {
                'O' => '0',
                '0' => 'O',
                'I' => '1',
                '1' => 'I',
                'S' => '5',
                '5' => 'S',
                'B' => '8',
                '8' => 'B',
                'Z' => '2',
                '2' => 'Z',
                x => {
                    let mut y = x;
                    while y == x {
                        y = *rng.pick(&"ABCDEFGHJKLMNPQRSTUVWXYZ23456789".chars().collect::<Vec<_>>());
                    }
                    y
                }
            };
            c[i] = look;
        }
        1 => {
            let i = rng.range(0, c.len() as i64 - 2) as usize;
            c.swap(i, i + 1);
            if c.iter().collect::<String>() == body {
                c[i] = if c[i] == 'X' { 'Y' } else { 'X' };
            }
        }
        _ => {
            let i = rng.range(0, c.len() as i64 - 1) as usize;
            c.remove(i);
        }
    }
    c.into_iter().collect()
}

fn reformat(rng: &mut Rng, reference: &str) -> String {
    let body = &reference[3..];
    match rng.range(0, 3) {
        0 => reference.to_lowercase(),
        1 => format!("IR{body}"),
        2 => format!("IR {body}"),
        _ => format!("IR-{} {}", &body[..3], &body[3..]),
    }
}

fn variant_of(rng: &mut Rng, listed: &str) -> String {
    let swaps = [
        ("Mohammed", "Muhammad"),
        ("Yusuf", "Youssef"),
        ("Sergei", "Sergey"),
        ("Dmitry", "Dmitri"),
        ("Hussein", "Hossein"),
        ("Aleksandr", "Alexander"),
        ("Yuri", "Iurii"),
        ("Mikhail", "Michail"),
        ("Abdullah", "Abdallah"),
        ("Trading", "Trdg"),
        ("International", "Intl"),
        ("Company", "Co"),
    ];
    for (a, b) in swaps {
        if listed.contains(a) && rng.chance(0.7) {
            return listed.replace(a, b);
        }
    }
    let words: Vec<&str> = listed.split_whitespace().collect();
    match rng.range(0, 2) {
        0 if words.len() >= 3 => format!("{} {}", words[0], words[words.len() - 1]), // drop the middle name
        1 => {
            let mut w = words.clone();
            w.reverse();
            w.join(" ")
        }
        _ => {
            // a one-letter slip in the longest word
            let (i, _) = words.iter().enumerate().max_by_key(|(_, w)| w.len()).unwrap();
            let mut ws: Vec<String> = words.iter().map(|s| s.to_string()).collect();
            let mut ch: Vec<char> = ws[i].chars().collect();
            let k = ch.len() / 2;
            ch[k] = if ch[k] == 'e' { 'a' } else { 'e' };
            ws[i] = ch.into_iter().collect();
            ws.join(" ")
        }
    }
}

fn near_miss(rng: &mut Rng, listed: &str) -> String {
    // An innocent person who shares a surname, or a first name, with a listed one.
    let words: Vec<&str> = listed.split_whitespace().collect();
    if rng.chance(0.5) { format!("{} {}", rng.pick(FIRST), words[words.len() - 1]) } else { format!("{} {}", words[0], rng.pick(LAST)) }
}

pub fn generate(cfg: &WorldCfg) -> World {
    let mut rng = Rng::new(cfg.seed);
    let blz = "10010010";
    let routing_ours = "02100002".to_string();
    let eur_pool = de_iban(blz, "9000000000");
    let usd_pool = "8899999999".to_string();
    let bank_accounts =
        vec![(Ccy::EUR, de_iban(blz, "9100000001"), de_iban(blz, "9100000002")), (Ccy::USD, "9200000001".to_string(), "9200000002".to_string())];

    // Customers
    let mut used_names = BTreeSet::new();
    let mut customers = vec![];
    let mut customer_payers = vec![];
    let mut platform_flags = vec![];
    for i in 0..cfg.customers {
        let name = loop {
            let n = format!("{} {} {}", rng.pick(PREFIX), rng.pick(INDUSTRY), rng.pick(FORMS));
            let key: String = n.split_whitespace().take(2).collect();
            if used_names.insert(key) {
                break n;
            }
        };
        let id = format!("cus-{:03}", i + 1);
        let status = if i < cfg.edd_customers {
            CustomerStatus::Edd
        } else if i < cfg.edd_customers + cfg.suspended_customers {
            CustomerStatus::Suspended
        } else {
            CustomerStatus::Active
        };
        let eth = format!("0x{:016x}{:016x}{:08x}", rng.u64(), rng.u64(), rng.u64() as u32);
        let sol = format!("So{}", rng.alnum(40));
        let usd_payout = rng.chance(cfg.usd_share);
        let payout = if usd_payout { (Rail::Ach, rng.digits(10)) } else { (Rail::Sepa, de_iban("50010517", &rng.digits(10))) };
        let base: String = name.split_whitespace().take_while(|w| !FORMS.contains(w)).collect::<Vec<_>>().join(" ");
        let own_eur = de_iban(&rng.digits(8), &rng.digits(10));
        let own_usd = rng.digits(12);
        let payers = vec![
            Payer { name: name.clone(), account: own_eur.clone() },
            Payer { name: name.to_uppercase(), account: own_eur },
            Payer { name: base.clone(), account: own_usd.clone() },
            Payer { name: format!("{base} {}", if name.contains("Ltd") { "Limited" } else { "Ltd" }), account: own_usd },
        ];
        customers.push(Customer {
            id: id.clone(),
            legal_name: name.clone(),
            trading_names: vec![base],
            status,
            wallets: vec![("ethereum".into(), eth), ("solana".into(), sol)],
            payout_account: payout,
        });
        customer_payers.push((id, payers));
        platform_flags.push(rng.chance(cfg.platform_customer_share));
    }

    // Autoramps
    let mut autoramps: Vec<Autoramp> = vec![];
    let mut used_refs = BTreeSet::new();
    let mut ar_seq = 0;
    for (ci, c) in customers.iter().enumerate() {
        let n = rng.range(1, 3);
        for k in 0..n {
            ar_seq += 1;
            let ccy = if rng.chance(cfg.usd_share) { Ccy::USD } else { Ccy::EUR };
            let pbr = rng.chance(cfg.pay_by_reference_share);
            let chain = if rng.chance(0.6) { "ethereum" } else { "solana" };
            let wallet = c.wallets.iter().find(|(ch, _)| ch == chain).unwrap().1.clone();
            let reference = if pbr {
                Some(loop {
                    let r = format!("IR-{}", rng.alnum(6));
                    if used_refs.insert(r.clone()) {
                        break r;
                    }
                })
            } else {
                None
            };
            let deposit_account = match (pbr, ccy) {
                (true, Ccy::EUR) => eur_pool.clone(),
                (true, Ccy::USD) => usd_pool.clone(),
                (false, Ccy::EUR) => de_iban(blz, &format!("80{:08}", ar_seq)),
                (false, Ccy::USD) => format!("88{:08}", ar_seq),
            };
            let status = if k == n - 1 && n > 1 && rng.chance(0.25) { AutorampStatus::Cancelled } else { AutorampStatus::Approved };
            autoramps.push(Autoramp {
                id: format!("ar-{:04}", ar_seq),
                customer_id: c.id.clone(),
                ccy,
                chain: chain.into(),
                wallet,
                deposit_account,
                pay_by_reference: pbr,
                reference,
                third_party: platform_flags[ci],
                status,
            });
        }
    }
    // Near-twin references: pairs of live pay-by-reference Autoramps one character apart, so that a
    // typo can sit one edit away from both.
    let twin_sources: Vec<usize> =
        autoramps.iter().enumerate().filter(|(_, a)| a.pay_by_reference && a.status == AutorampStatus::Approved).map(|(i, _)| i).take(4).collect();
    for &i in &twin_sources {
        let src = autoramps[i].clone();
        let body = &src.reference.as_ref().unwrap()[3..];
        let mut twin_body: Vec<char> = body.chars().collect();
        twin_body[5] = if twin_body[5] == 'Q' { 'R' } else { 'Q' };
        let r = format!("IR-{}", twin_body.iter().collect::<String>());
        if !used_refs.insert(r.clone()) {
            continue;
        }
        ar_seq += 1;
        let other = customers.iter().filter(|c| c.id != src.customer_id && c.status == CustomerStatus::Active).nth((rng.u64() % 10) as usize).unwrap();
        autoramps.push(Autoramp {
            id: format!("ar-{:04}", ar_seq),
            customer_id: other.id.clone(),
            ccy: src.ccy,
            chain: src.chain.clone(),
            wallet: other.wallets[0].1.clone(),
            deposit_account: src.deposit_account.clone(),
            pay_by_reference: true,
            reference: Some(r),
            third_party: false,
            status: AutorampStatus::Approved,
        });
    }

    let mut sanctions = vec![];
    for (i, n) in LISTED_PEOPLE.iter().enumerate() {
        sanctions.push(SanctionsEntry { id: format!("SL-P{:03}", i + 1), names: vec![n.to_string()], kind: "individual".into() });
    }
    for (i, n) in LISTED_ENTITIES.iter().enumerate() {
        sanctions.push(SanctionsEntry { id: format!("SL-E{:03}", i + 1), names: vec![n.to_string()], kind: "entity".into() });
    }

    let platform = Platform {
        customers,
        autoramps,
        sanctions,
        bank_accounts,
        redemption_addresses: vec![
            ("ethereum".into(), "0x000000000000000000000000000000000000dEaD".into()),
            ("solana".into(), "RedeemXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX".into()),
        ],
    };
    let payments = plan_payments(cfg, &platform, &customer_payers, &mut rng, &eur_pool, &usd_pool, blz);
    World { cfg: cfg.clone(), platform, payments, customer_payers, routing_ours }
}

fn stranger(rng: &mut Rng) -> Payer {
    Payer { name: format!("{} {}", rng.pick(FIRST), rng.pick(LAST)), account: de_iban(&rng.digits(8), &rng.digits(10)) }
}

#[allow(clippy::too_many_arguments)]
fn plan_payments(
    cfg: &WorldCfg,
    pl: &Platform,
    payers: &[(String, Vec<Payer>)],
    rng: &mut Rng,
    eur_pool: &str,
    usd_pool: &str,
    blz: &str,
) -> Vec<PlannedPayment> {
    let days = business_days(cfg);
    let live: Vec<&Autoramp> = pl
        .autoramps
        .iter()
        .filter(|a| a.status == AutorampStatus::Approved && pl.customer(&a.customer_id).unwrap().status != CustomerStatus::Suspended)
        .collect();
    let cancelled: Vec<&Autoramp> = pl.autoramps.iter().filter(|a| a.status == AutorampStatus::Cancelled).collect();
    let suspended_ars: Vec<&Autoramp> = pl.autoramps.iter().filter(|a| pl.customer(&a.customer_id).unwrap().status == CustomerStatus::Suspended).collect();
    let pbr_live: Vec<&Autoramp> = live.iter().copied().filter(|a| a.pay_by_reference).collect();
    let twins: Vec<(&Autoramp, &Autoramp)> = pbr_live
        .iter()
        .flat_map(|a| pbr_live.iter().map(move |b| (*a, *b)))
        .filter(|(a, b)| {
            a.id < b.id
                && a.deposit_account == b.deposit_account
                && crate::routing::osa_distance(&a.reference.as_ref().unwrap()[3..], &b.reference.as_ref().unwrap()[3..]) == 1
        })
        .collect();
    let mut out: Vec<PlannedPayment> = vec![];
    let mut seq = 0u64;
    let mut special: Vec<(usize, Kind)> = vec![];
    let total = days.len() * cfg.deposits_per_day;
    // Counted planted cases are scattered over the month.
    for _ in 0..cfg.sanctioned_exact {
        special.push((rng.range(0, total as i64 - 1) as usize, Kind::SanctionedExact));
    }
    for _ in 0..cfg.sanctioned_variant {
        special.push((rng.range(0, total as i64 - 1) as usize, Kind::SanctionedVariant));
    }
    for _ in 0..cfg.near_miss_innocent {
        special.push((rng.range(0, total as i64 - 1) as usize, Kind::NearMissInnocent));
    }
    let statement_only: BTreeSet<usize> = (0..cfg.statement_only).map(|_| rng.range(0, total as i64 - 1) as usize).collect();
    let listed: Vec<String> = pl.sanctions.iter().map(|s| s.names[0].clone()).collect();

    for (di, &day) in days.iter().enumerate() {
        for k in 0..cfg.deposits_per_day {
            let n = di * cfg.deposits_per_day + k;
            let t = day * DAY + rng.range(7 * HOUR, 21 * HOUR);
            let mut kind = {
                let r = rng.f64();
                let mut acc = 0.0;
                let table = [
                    (cfg.p_reformatted, Kind::ReferenceReformatted),
                    (cfg.p_typo_customer, Kind::ReferenceTypoByCustomer),
                    (cfg.p_typo_stranger, Kind::ReferenceTypoByStranger),
                    (cfg.p_typo_ambiguous, Kind::ReferenceTypoAmbiguous),
                    (cfg.p_missing_reference, Kind::ReferenceMissing),
                    (cfg.p_cancelled, Kind::CancelledAutoramp),
                    (cfg.p_unknown_account, Kind::UnknownAccount),
                    (cfg.p_resend, Kind::CustomerResend),
                    (cfg.p_legit_repeat, Kind::LegitRepeat),
                    (cfg.p_third_party_not_allowed, Kind::ThirdPartyNotAllowed),
                    (cfg.p_below_minimum, Kind::BelowMinimum),
                ];
                let mut k = Kind::Clean;
                for (p, kk) in table {
                    acc += p;
                    if r < acc {
                        k = kk;
                        break;
                    }
                }
                k
            };
            if let Some((_, sk)) = special.iter().find(|(i, _)| *i == n) {
                kind = sk.clone();
            }
            // Pick the Autoramp the sender means to pay.
            let mut ar: &Autoramp = match &kind {
                Kind::ReferenceReformatted | Kind::ReferenceTypoByCustomer | Kind::ReferenceTypoByStranger | Kind::ReferenceMissing => {
                    rng.pick::<&Autoramp>(&pbr_live)
                }
                Kind::ReferenceTypoAmbiguous if !twins.is_empty() => rng.pick(&twins).0,
                Kind::CancelledAutoramp if !suspended_ars.is_empty() && rng.chance(0.3) => {
                    kind = Kind::SuspendedCustomer;
                    rng.pick::<&Autoramp>(&suspended_ars)
                }
                Kind::CancelledAutoramp if !cancelled.is_empty() => rng.pick::<&Autoramp>(&cancelled),
                Kind::ThirdPartyNotAllowed => loop {
                    let a = *rng.pick(&live);
                    if !a.third_party {
                        break a;
                    }
                },
                Kind::NearMissInnocent => loop {
                    let a = *rng.pick(&live);
                    if a.third_party {
                        break a;
                    }
                },
                _ => rng.pick::<&Autoramp>(&live),
            };
            if matches!(kind, Kind::ReferenceTypoAmbiguous) && twins.is_empty() {
                kind = Kind::ReferenceTypoByStranger;
            }
            if matches!(kind, Kind::CancelledAutoramp) && cancelled.is_empty() {
                kind = Kind::Clean;
            }
            let cust = pl.customer(&ar.customer_id).unwrap();
            if cust.status == CustomerStatus::Edd && matches!(kind, Kind::Clean | Kind::ReferenceExact) {
                kind = Kind::EddCustomer;
            }
            let own = &payers.iter().find(|(id, _)| *id == ar.customer_id).unwrap().1;
            let mut payer = match &kind {
                Kind::ReferenceTypoByStranger | Kind::ThirdPartyNotAllowed => stranger(rng),
                Kind::SanctionedExact => Payer { name: rng.pick(&listed).clone(), account: de_iban(&rng.digits(8), &rng.digits(10)) },
                Kind::SanctionedVariant => {
                    let l = rng.pick(&listed).clone();
                    Payer { name: variant_of(rng, &l), account: de_iban(&rng.digits(8), &rng.digits(10)) }
                }
                Kind::NearMissInnocent => {
                    let l = rng.pick(LISTED_PEOPLE).to_string();
                    Payer { name: near_miss(rng, &l), account: de_iban(&rng.digits(8), &rng.digits(10)) }
                }
                // Customers who accept third parties are often paid by them; every other planted
                // case is sent by the customer itself.
                Kind::Clean if ar.third_party && rng.chance(0.6) => {
                    kind = Kind::ThirdPartyAllowed;
                    stranger(rng)
                }
                _ => rng.pick(own).clone(),
            };
            if matches!(kind, Kind::SanctionedVariant) && listed.contains(&payer.name) {
                kind = Kind::SanctionedExact;
            }
            // Rail and IDs.
            let ccy = ar.ccy;
            let rail = match ccy {
                Ccy::EUR => {
                    if rng.chance(cfg.eur_swift_share) {
                        Rail::Swift
                    } else {
                        Rail::Sepa
                    }
                }
                Ccy::USD => {
                    let r = rng.f64();
                    if r < cfg.usd_rail_mix[0] {
                        Rail::Ach
                    } else if r < cfg.usd_rail_mix[0] + cfg.usd_rail_mix[1] {
                        Rail::Fedwire
                    } else {
                        Rail::Swift
                    }
                }
            };
            if rail == Rail::Ach || rail == Rail::Fedwire {
                payer.account = rng.digits(12);
            }
            let amount_minor = if matches!(kind, Kind::BelowMinimum) { rng.range(1, 99) } else { amount(rng) };
            let mut creditor_account = ar.deposit_account.clone();
            let mut reference_text = ar.reference.clone();
            match &kind {
                Kind::ReferenceReformatted => reference_text = Some(reformat(rng, ar.reference.as_ref().unwrap())),
                Kind::ReferenceTypoByCustomer | Kind::ReferenceTypoByStranger => {
                    // A typo that lands one edit from this reference and no other live one.
                    let mut tries = 0;
                    loop {
                        let body = mutate_one(rng, &ar.reference.as_ref().unwrap()[3..]);
                        let close = pbr_live
                            .iter()
                            .filter(|o| {
                                o.deposit_account == ar.deposit_account && crate::routing::osa_distance(&body, &o.reference.as_ref().unwrap()[3..]) <= 1
                            })
                            .count();
                        tries += 1;
                        if close == 1 || tries > 20 {
                            reference_text = Some(format!("IR-{body}"));
                            break;
                        }
                    }
                }
                Kind::ReferenceTypoAmbiguous => {
                    let (a, b) = twins.iter().find(|(a, _)| a.id == ar.id).copied().unwrap_or(twins[0]);
                    ar = a;
                    let ab: Vec<char> = a.reference.as_ref().unwrap()[3..].chars().collect();
                    let bb: Vec<char> = b.reference.as_ref().unwrap()[3..].chars().collect();
                    let pos = (0..6).find(|&i| ab[i] != bb[i]).unwrap();
                    let mut x = ab.clone();
                    x[pos] = ['M', 'N', 'P', 'W'].into_iter().find(|c| *c != ab[pos] && *c != bb[pos]).unwrap();
                    reference_text = Some(format!("IR-{}", x.iter().collect::<String>()));
                    creditor_account = a.deposit_account.clone();
                }
                Kind::ReferenceMissing => reference_text = None,
                Kind::UnknownAccount => {
                    creditor_account = if ccy == Ccy::EUR { de_iban(blz, &format!("80{}", rng.digits(8))) } else { format!("88{}", rng.digits(8)) };
                    if pl.autoramps.iter().any(|a| a.deposit_account == creditor_account) {
                        creditor_account = if ccy == Ccy::EUR { de_iban(blz, "7999999999") } else { "8800000000".into() };
                    }
                }
                _ => {}
            }
            let _ = (eur_pool, usd_pool);
            let cust_on_suspended = suspended_ars.iter().any(|a| a.id == ar.id);
            seq += 1;
            let mut ids = TraceIds { end_to_end_id: Some(format!("E2E{}", rng.alnum(12))), ..Default::default() };
            match rail {
                Rail::Sepa => ids.acct_svcr_ref = Some(format!("{}{:08}", fmt_date(t).replace('-', ""), seq)),
                Rail::Swift => ids.uetr = Some(rng.uuid()),
                Rail::Ach => {
                    ids.ach_trace = Some(format!("{}{:07}", rng.digits(8), seq % 10_000_000));
                    ids.end_to_end_id = None;
                }
                Rail::Fedwire => ids.imad = Some(format!("{}QMGFT{}{:06}", fmt_date(t).replace('-', ""), rng.digits(3), seq % 1_000_000)),
            }
            let bank_ref = crate::rails::iso::bank_ref_for(rail, &ids).unwrap();
            let invoice = format!("Invoice {}", rng.digits(5));
            let (structured, unstructured) = match (&reference_text, rail) {
                (Some(r), Rail::Sepa) if rng.chance(0.3) => (Some(r.clone()), Some(invoice.clone())),
                (Some(r), Rail::Ach) => (None, Some(format!("{invoice} {r}"))),
                (Some(r), _) => (None, Some(if rng.chance(0.5) { format!("{r} {invoice}") } else { format!("Payment for {invoice} ref {r}") })),
                (None, _) => (None, Some(invoice.clone())),
            };
            let payment = InboundPayment {
                rail,
                bank_ref: bank_ref.clone(),
                ids: ids.clone(),
                amount_minor,
                ccy,
                booked_at: t,
                debtor_name: payer.name.clone(),
                debtor_account: payer.account.clone(),
                creditor_account,
                structured_ref: structured,
                unstructured,
            };
            let (expected, holds, decision) = expectation(&kind, cust.status, ar, cust_on_suspended);
            let notify = if statement_only.contains(&n) && matches!(rail, Rail::Sepa | Rail::Fedwire) {
                Notify::StatementOnly
            } else {
                match rail {
                    Rail::Sepa => Notify::Camt054,
                    Rail::Swift => Notify::Pacs008,
                    Rail::Ach => Notify::AchFile,
                    Rail::Fedwire => Notify::Fedwire,
                }
            };
            let renotify = matches!(rail, Rail::Sepa | Rail::Swift) && rng.chance(cfg.p_renotify);
            let truth = DepositTruth {
                rail,
                bank_ref: bank_ref.clone(),
                kind: kind.clone(),
                amount_minor,
                ccy,
                intended_autoramp: if matches!(kind, Kind::UnknownAccount) { None } else { Some(ar.id.clone()) },
                expected,
                expected_holds: holds,
                review_decision: decision,
                resend_of: None,
                statement_only: matches!(notify, Notify::StatementOnly),
                trace_ids: ids.all().into_iter().map(|(_, v)| v.to_string()).collect(),
            };
            out.push(PlannedPayment { payment, notify, renotify, truth });

            // The customer's accidental second payment, or a genuine second invoice for the same amount.
            if matches!(kind, Kind::CustomerResend | Kind::LegitRepeat) {
                let first = out.last().unwrap().clone();
                let mut p2 = first.payment.clone();
                let later = (t + rng.range(3 * HOUR, 72 * HOUR)).max(t + HOUR);
                let later = if weekday(later.div_euclid(DAY)) >= 5 {
                    (later.div_euclid(DAY) + (7 - weekday(later.div_euclid(DAY)) as i64)) * DAY + 9 * HOUR
                } else {
                    later
                };
                p2.booked_at = later;
                seq += 1;
                let mut ids2 = TraceIds { end_to_end_id: Some(format!("E2E{}", rng.alnum(12))), ..Default::default() };
                match rail {
                    Rail::Sepa => ids2.acct_svcr_ref = Some(format!("{}{:08}", fmt_date(later).replace('-', ""), seq)),
                    Rail::Swift => ids2.uetr = Some(rng.uuid()),
                    Rail::Ach => {
                        ids2.ach_trace = Some(format!("{}{:07}", rng.digits(8), seq % 10_000_000));
                        ids2.end_to_end_id = None;
                    }
                    Rail::Fedwire => ids2.imad = Some(format!("{}QMGFT{}{:06}", fmt_date(later).replace('-', ""), rng.digits(3), seq % 1_000_000)),
                }
                p2.bank_ref = crate::rails::iso::bank_ref_for(rail, &ids2).unwrap();
                p2.ids = ids2.clone();
                let is_resend = matches!(kind, Kind::CustomerResend);
                let (exp2, holds2, dec2) = if cust.status == CustomerStatus::Edd {
                    (
                        Expected::Hold,
                        vec!["customer_in_edd".to_string(), "possible_resend".to_string()],
                        Some(if is_resend { "return" } else { "release" }.to_string()),
                    )
                } else if is_resend {
                    (Expected::Hold, vec!["possible_resend".to_string()], Some("return".to_string()))
                } else {
                    (Expected::Credit, vec![], Some("release".to_string()))
                };
                let t2 = DepositTruth {
                    bank_ref: p2.bank_ref.clone(),
                    kind: kind.clone(),
                    expected: exp2,
                    expected_holds: holds2,
                    review_decision: dec2,
                    resend_of: Some(first.truth.bank_ref.clone()),
                    statement_only: false,
                    trace_ids: ids2.all().into_iter().map(|(_, v)| v.to_string()).collect(),
                    ..first.truth.clone()
                };
                // The first of the pair is an ordinary payment.
                let last = out.len() - 1;
                let (e1, h1, d1) = expectation(&Kind::Clean, cust.status, ar, cust_on_suspended);
                out[last].truth.kind = if ar.pay_by_reference { Kind::ReferenceExact } else { Kind::Clean };
                out[last].truth.expected = e1;
                out[last].truth.expected_holds = h1;
                out[last].truth.review_decision = d1;
                let notify2 = match rail {
                    Rail::Sepa => Notify::Camt054,
                    Rail::Swift => Notify::Pacs008,
                    Rail::Ach => Notify::AchFile,
                    Rail::Fedwire => Notify::Fedwire,
                };
                out.push(PlannedPayment { payment: p2, notify: notify2, renotify: false, truth: t2 });
            } else if matches!(kind, Kind::Clean) && ar.pay_by_reference {
                let last = out.len() - 1;
                out[last].truth.kind = Kind::ReferenceExact;
            }
        }
    }
    out.sort_by_key(|p| p.payment.booked_at);
    out
}

/// What a correct engine does with a deposit of this kind.
fn expectation(kind: &Kind, status: CustomerStatus, ar: &Autoramp, on_suspended: bool) -> (Expected, Vec<String>, Option<String>) {
    let edd = status == CustomerStatus::Edd;
    let with_edd = |mut v: Vec<String>| {
        if edd {
            v.push("customer_in_edd".into());
        }
        v
    };
    if on_suspended && !matches!(kind, Kind::UnknownAccount) {
        return (Expected::Return, vec![], None);
    }
    match kind {
        Kind::Clean
        | Kind::ReferenceExact
        | Kind::ReferenceReformatted
        | Kind::ReferenceTypoByCustomer
        | Kind::LegitRepeat
        | Kind::ThirdPartyAllowed
        | Kind::NearMissInnocent => {
            if edd {
                (Expected::Hold, with_edd(vec![]), Some("release".into()))
            } else {
                (Expected::Credit, vec![], None)
            }
        }
        Kind::EddCustomer => (Expected::Hold, vec!["customer_in_edd".into()], Some("release".into())),
        Kind::ReferenceTypoByStranger
        | Kind::ReferenceTypoAmbiguous
        | Kind::ReferenceMissing
        | Kind::CancelledAutoramp
        | Kind::UnknownAccount
        | Kind::SuspendedCustomer => (Expected::Return, vec![], None),
        Kind::CustomerResend => (Expected::Hold, with_edd(vec!["possible_resend".into()]), Some("return".into())),
        Kind::ThirdPartyNotAllowed => (Expected::Hold, with_edd(vec!["third_party_sender".into()]), Some("return".into())),
        Kind::SanctionedExact | Kind::SanctionedVariant => {
            let mut h = vec!["sanctions_hit".to_string()];
            if !ar.third_party {
                h.push("third_party_sender".into());
            }
            (Expected::Hold, with_edd(h), Some("reject".into()))
        }
        Kind::BelowMinimum => (Expected::RejectMin, vec![], None),
    }
}

pub fn minutes(n: i64) -> Ts {
    n * MINUTE
}

pub fn day_start(day: i64) -> Ts {
    day * DAY
}

pub fn first_day(cfg: &WorldCfg) -> i64 {
    business_days(cfg)[0]
}

pub fn last_day(cfg: &WorldCfg) -> i64 {
    *business_days(cfg).last().unwrap()
}

pub fn civil(y: i64, m: u32, d: u32) -> i64 {
    days_from_civil(y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ibans_have_valid_check_digits() {
        let i = de_iban("37040044", "0532013000");
        assert_eq!(i, "DE89370400440532013000");
        assert!(iban_valid(&i));
        assert!(!iban_valid("DE88370400440532013000"));
    }

    #[test]
    fn rng_is_deterministic() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        assert_eq!((0..5).map(|_| a.u64()).collect::<Vec<_>>(), (0..5).map(|_| b.u64()).collect::<Vec<_>>());
        assert_eq!(a.uuid().len(), 36);
    }
}
