//! Randomised checks of the routing rules: reading references through formatting never fails, and a
//! repair never lands on a customer the sender did not mean.

use stablecoin_payin_engine::domain::*;
use stablecoin_payin_engine::money::Ccy;
use stablecoin_payin_engine::names::Normaliser;
use stablecoin_payin_engine::routing::{MatchHow, Route, Router, osa_distance};
use stablecoin_payin_engine::sim::world::Rng;

fn pool(rng: &mut Rng, n: usize) -> Platform {
    let mut autoramps = vec![];
    let mut customers = vec![];
    for i in 0..n {
        let body = rng.alnum(6);
        customers.push(Customer {
            id: format!("c{i}"),
            legal_name: format!("Customer Number {i} GmbH"),
            trading_names: vec![],
            status: CustomerStatus::Active,
            wallets: vec![],
            payout_account: (Rail::Sepa, "x".into()),
        });
        autoramps.push(Autoramp {
            id: format!("a{i}"),
            customer_id: format!("c{i}"),
            ccy: Ccy::EUR,
            chain: "ethereum".into(),
            wallet: "w".into(),
            deposit_account: "POOL".into(),
            pay_by_reference: true,
            reference: Some(format!("IR-{body}")),
            third_party: i % 3 == 0,
            status: AutorampStatus::Approved,
        });
        // A near twin now and then, so typos can be ambiguous.
        if rng.chance(0.2) {
            let mut t: Vec<char> = body.chars().collect();
            t[0] = if t[0] == 'A' { 'B' } else { 'A' };
            let j = autoramps.len();
            customers.push(Customer {
                id: format!("t{j}"),
                legal_name: format!("Twin Holder {j} Ltd"),
                trading_names: vec![],
                status: CustomerStatus::Active,
                wallets: vec![],
                payout_account: (Rail::Sepa, "x".into()),
            });
            autoramps.push(Autoramp {
                id: format!("t{j}"),
                customer_id: format!("t{j}"),
                ccy: Ccy::EUR,
                chain: "ethereum".into(),
                wallet: "w".into(),
                deposit_account: "POOL".into(),
                pay_by_reference: true,
                reference: Some(format!("IR-{}", t.iter().collect::<String>())),
                third_party: false,
                status: AutorampStatus::Approved,
            });
        }
    }
    Platform { customers, autoramps, sanctions: vec![], bank_accounts: vec![], redemption_addresses: vec![] }
}

fn pay(text: String, sender: &str) -> InboundPayment {
    InboundPayment {
        rail: Rail::Sepa,
        bank_ref: "r".into(),
        ids: TraceIds::default(),
        amount_minor: 100_00,
        ccy: Ccy::EUR,
        booked_at: 0,
        debtor_name: sender.into(),
        debtor_account: "x".into(),
        creditor_account: "POOL".into(),
        structured_ref: None,
        unstructured: Some(text),
    }
}

#[test]
fn formatting_never_hides_a_reference() {
    let cfg = stablecoin_payin_engine::config::load_named("engine").unwrap();
    let n = Normaliser::new(&cfg.names.variants);
    let mut rng = Rng::new(42);
    let pl = pool(&mut rng, 200);
    let router = Router { cfg: &cfg, platform: &pl, names: &n };
    for a in &pl.autoramps {
        let r = a.reference.clone().unwrap();
        let body = &r[3..];
        for text in [r.to_lowercase(), format!("IR{body}"), format!("ir {body}"), format!("Invoice 7 IR-{} {} thanks", &body[..2], &body[2..])] {
            match router.route(&pay(text.clone(), "anyone")) {
                Route::Matched { autoramp_id, .. } => assert_eq!(autoramp_id, a.id, "{text}"),
                other => panic!("{text}: {other:?}"),
            }
        }
    }
}

#[test]
fn a_repair_never_lands_on_someone_else() {
    let cfg = stablecoin_payin_engine::config::load_named("engine").unwrap();
    let n = Normaliser::new(&cfg.names.variants);
    let mut rng = Rng::new(7);
    let pl = pool(&mut rng, 300);
    let router = Router { cfg: &cfg, platform: &pl, names: &n };
    let alphabet: Vec<char> = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789".chars().collect();
    let mut repaired = 0;
    for _ in 0..5000 {
        let a = rng.pick(&pl.autoramps).clone();
        let mut body: Vec<char> = a.reference.as_ref().unwrap()[3..].chars().collect();
        // one or two random substitutions
        for _ in 0..rng.range(1, 2) {
            let i = rng.range(0, 5) as usize;
            body[i] = *rng.pick(&alphabet);
        }
        let typed: String = body.iter().collect();
        let sender = pl.customer(&a.customer_id).unwrap().legal_name.clone();
        if let Route::Matched { autoramp_id, how } = router.route(&pay(format!("IR-{typed}"), &sender)) {
            if matches!(how, MatchHow::ReferenceRepaired { .. }) {
                repaired += 1;
            }
            let target = pl.autoramp(&autoramp_id).unwrap().reference.clone().unwrap();
            // Matching is allowed only to the meant Autoramp, or to one the typed text equals exactly.
            assert!(autoramp_id == a.id || target[3..] == typed, "IR-{typed} meant {} landed on {autoramp_id}", a.id);
            assert!(osa_distance(&typed, &target[3..]) <= 1, "typed IR-{typed} target {target} how {how:?}");
        }
        // From a stranger, a near miss is never repaired.
        if let Route::Matched { how: MatchHow::ReferenceRepaired { .. }, .. } = router.route(&pay(format!("IR-{typed}"), "Somebody Else Entirely")) {
            panic!("repaired a stranger's reference");
        }
    }
    assert!(repaired > 1000, "the check is meaningful only if many repairs happen ({repaired})");
}
