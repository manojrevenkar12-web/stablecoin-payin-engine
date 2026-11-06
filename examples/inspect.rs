//! Lists the dev world's deposits of one planted kind with what the engine did: `cargo run --example inspect -- engine ReferenceTypoByCustomer`.
//! Dev world only; held-out failures are never studied.
use stablecoin_payin_engine::{config, sim};
fn main() {
    let policy = std::env::args().nth(1).unwrap_or("engine".into());
    let mut wc = sim::world::WorldCfg::load("world").unwrap();
    if let Some(s) = std::env::args().nth(3) {
        wc.seed = s.parse().unwrap();
    }
    let w = sim::world::generate(&wc);
    let out = sim::run::run(&w, config::load_named(&policy).unwrap(), false);
    let e = &out.engine;
    for d in &out.truth.deposits {
        let Some(dep) = e.state.deposits.iter().find(|x| x.payment.bank_ref == d.bank_ref) else {
            println!("MISSING {:?}", d.kind);
            continue;
        };
        let tx = dep.transaction_id.as_ref().and_then(|t| e.transaction(t));
        let status = tx.map(|t| format!("{:?}", t.status)).unwrap_or("-".into());
        let holds: Vec<String> =
            tx.and_then(|t| t.hold_id.as_ref()).and_then(|h| e.hold(h)).map(|h| h.reasons.iter().map(|r| r.code().to_string()).collect()).unwrap_or_default();
        let want = std::env::args().nth(2).unwrap_or_default();
        if format!("{:?}", d.kind) == want {
            let cust = d
                .intended_autoramp
                .as_ref()
                .and_then(|a| w.platform.autoramp(a))
                .and_then(|a| w.platform.customer(&a.customer_id))
                .map(|c| c.legal_name.clone());
            println!(
                "{} | cust {:?} | {:?} {:?} | ar {:?} | {:?} | {} | {:?} | exp {:?}",
                dep.payment.debtor_name, cust, dep.payment.structured_ref, dep.payment.unstructured, d.intended_autoramp, dep.route, status, holds, d.expected
            );
        }
    }
}
