//! The engine against a whole simulated month: the invariants that must hold whatever the policy.

use stablecoin_payin_engine::{config, eval, sim};

fn run(policy: &str, seed: u64) -> (sim::run::RunOutput, eval::Results) {
    let mut wc = sim::world::WorldCfg::load("world").unwrap();
    wc.seed = seed;
    let w = sim::world::generate(&wc);
    let out = sim::run::run(&w, config::load_named(policy).unwrap(), false);
    let r = eval::evaluate(&out, "dev", seed);
    (out, r)
}

#[test]
fn engine_never_misroutes_and_the_books_close() {
    let (_, r) = run("engine", 11);
    assert_eq!(r.misrouted.n, 0, "credited to the wrong customer");
    assert_eq!(r.duplicate_mints.n, 0);
    assert!(r.trial_balance_zero);
    assert!(r.min_reserve_headroom_minor.values().all(|v| *v >= 0), "reserve went short: {:?}", r.min_reserve_headroom_minor);
    assert_eq!(r.unexplained_balance_breaks, 0);
    assert_eq!(r.chain_supply_breaks, 0);
    assert_eq!(r.reserve_coverage_breaks, 0);
    assert_eq!(r.deposits_booked_once.hit, r.deposits_booked_once.of);
    assert_eq!(r.corrupt_files_quarantined.hit, r.corrupt_files_quarantined.of);
    assert_eq!(r.mints_final_once.hit, r.mints_final_once.of);
    assert_eq!(r.reorgs_past_confirmation, 0);
    assert_eq!(r.usd_conversions_outside_window, 0);
    assert_eq!(r.trace_all.hit, r.trace_all.of);
}

#[test]
fn the_baseline_also_never_misroutes_but_returns_and_double_mints() {
    // The strict reading of the rules is safe from misroutes; what it costs is returned money and
    // second payments minted twice. The comparison is only fair if that holds.
    let (_, r) = run("baseline", 11);
    assert_eq!(r.misrouted.n, 0);
    assert!(r.trial_balance_zero);
    assert!(r.wrongly_returned.n > 0);
    assert!(r.duplicate_mints.n > 0);
}

#[test]
fn same_inputs_same_books() {
    let (a, _) = run("engine", 12);
    let (b, _) = run("engine", 12);
    let sa = serde_json::to_string(&a.engine.state.ledger.journals).unwrap();
    let sb = serde_json::to_string(&b.engine.state.ledger.journals).unwrap();
    assert_eq!(sa, sb, "the engine is deterministic");
}

#[test]
fn delivering_every_bank_file_twice_changes_nothing() {
    let mut wc = sim::world::WorldCfg::load("world").unwrap();
    wc.seed = 13;
    let w = sim::world::generate(&wc);
    let out = sim::run::run(&w, config::load_named("engine").unwrap(), true);
    let mut twice = stablecoin_payin_engine::engine::Engine::new(config::load_named("engine").unwrap(), w.platform.clone());
    for (at, input) in &out.inputs {
        twice.handle(*at, input.clone()).unwrap();
        if let stablecoin_payin_engine::engine::Input::BankFile { name, .. } = input {
            if !name.starts_with("camt053") {
                twice.handle(*at, input.clone()).unwrap();
            }
        }
        twice.drain();
    }
    let a: Vec<_> = out.engine.state.ledger.balances().map(|(k, v)| (k.clone(), *v)).collect();
    let b: Vec<_> = twice.state.ledger.balances().map(|(k, v)| (k.clone(), *v)).collect();
    assert_eq!(a, b);
    assert_eq!(out.engine.state.deposits.len(), twice.state.deposits.len());
}
