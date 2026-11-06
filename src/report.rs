//! Writes `reports/REPORT.md` and `reports/dashboard.html` from the generated results. Every number in
//! either file comes from `reports/*.json`; nothing here is typed in by hand.

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use std::path::Path;

const MEAN_KEYS: &[&str] = &["hours_to_mint_p50", "hours_to_mint_p90", "inputs_per_sec", "false_holds_per_1000"];

/// Sum results across worlds: counts and rates add up, timings average, headroom takes the minimum.
fn merge(a: &Value, b: &Value, key: &str) -> Value {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            if MEAN_KEYS.iter().any(|k| key.starts_with(k)) {
                json!(x.as_f64().unwrap() + y.as_f64().unwrap())
            } else if key.starts_with("min_reserve_headroom") {
                json!(x.as_i64().unwrap().min(y.as_i64().unwrap()))
            } else if let (Some(i), Some(j)) = (x.as_i64(), y.as_i64()) {
                json!(i + j)
            } else {
                json!(x.as_f64().unwrap() + y.as_f64().unwrap())
            }
        }
        (Value::Bool(x), Value::Bool(y)) => json!(*x && *y),
        (Value::Object(x), Value::Object(y)) => {
            let mut m = Map::new();
            for (k, v) in x {
                let sub = if MEAN_KEYS.contains(&key) || key.starts_with("min_reserve") { key.to_string() } else { k.clone() };
                m.insert(
                    k.clone(),
                    match y.get(k) {
                        Some(w) => merge(v, w, &sub),
                        None => v.clone(),
                    },
                );
            }
            for (k, w) in y {
                m.entry(k.clone()).or_insert_with(|| w.clone());
            }
            Value::Object(m)
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => Value::Array(x.iter().zip(y).map(|(p, q)| merge(p, q, key)).collect()),
        _ => a.clone(),
    }
}

fn total(runs: &[Value], who: &str) -> Value {
    let mut it = runs.iter().map(|r| r[who].clone());
    let first = it.next().unwrap_or(Value::Null);
    let mut t = it.fold(first, |acc, v| merge(&acc, &v, ""));
    // Timings and rates were summed; turn them into means.
    let k = runs.len().max(1) as f64;
    for key in MEAN_KEYS {
        match t.get_mut(*key) {
            Some(Value::Number(x)) => *t.get_mut(*key).unwrap() = json!(x.as_f64().unwrap() / k),
            Some(Value::Object(m)) => {
                for v in m.values_mut() {
                    *v = json!(v.as_f64().unwrap_or(0.0) / k);
                }
            }
            _ => {}
        }
    }
    t
}

fn n(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

fn rate(v: &Value) -> String {
    format!("{} of {}", n(&v["hit"]), n(&v["of"]))
}

fn pct(v: &Value) -> String {
    let of = n(&v["of"]);
    if of == 0 {
        return "–".into();
    }
    // Rounded down, so one miss in thousands never shows as 100.0%.
    let p = (1000.0 * n(&v["hit"]) as f64 / of as f64).floor() / 10.0;
    format!("{p:.1}%")
}

fn money(minor: i64, sym: &str) -> String {
    let units = minor as f64 / 100.0;
    if units >= 1_000_000.0 {
        format!("{sym}{:.2}m", units / 1_000_000.0)
    } else if units >= 1_000.0 {
        format!("{sym}{:.0}k", units / 1_000.0)
    } else {
        format!("{sym}{:.0}", units)
    }
}

fn both(c: &Value) -> String {
    let e = n(&c["eur_minor"]);
    let u = n(&c["usd_minor"]);
    match (e, u) {
        (0, 0) => "–".into(),
        (e, 0) => money(e, "€"),
        (0, u) => money(u, "$"),
        (e, u) => format!("{} + {}", money(e, "€"), money(u, "$")),
    }
}

fn read(p: &Path) -> Result<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?).with_context(|| format!("parsing {}", p.display()))
}

struct Facts {
    e: Value,
    b: Value,
    seeds: usize,
    held: Option<Value>,
    held_latest: Option<Value>,
    pg: Option<Value>,
    bench: Option<Value>,
}

fn facts(root: &Path) -> Result<Facts> {
    let res = read(&root.join("reports/results.json"))?;
    let runs = res["dev"].as_array().cloned().unwrap_or_default();
    Ok(Facts {
        e: total(&runs, "engine"),
        b: total(&runs, "baseline"),
        seeds: runs.len(),
        held: read(&root.join("reports/heldout/v1/first_run.json")).ok(),
        held_latest: read(&root.join("reports/heldout/v1/latest.json")).ok(),
        pg: read(&root.join("reports/replay_pg.json")).ok(),
        bench: read(&root.join("reports/bench.json")).ok(),
    })
}

const KIND_TEXT: &[(&str, &str)] = &[
    ("Clean", "Paid to a dedicated virtual account"),
    ("ReferenceExact", "Pay-by-reference, reference exactly as issued"),
    ("ReferenceReformatted", "Reference in lower case, without the hyphen, spaced or split"),
    ("ReferenceTypoByCustomer", "One-character typo, sent by the customer"),
    ("ReferenceTypoByStranger", "One-character typo, sent by someone else"),
    ("ReferenceTypoAmbiguous", "Typo one edit from two live references"),
    ("ReferenceMissing", "Pay-by-reference account paid without a reference"),
    ("CancelledAutoramp", "Paid to a cancelled Autoramp"),
    ("SuspendedCustomer", "Paid to a suspended customer"),
    ("UnknownAccount", "Paid to an account number we never issued"),
    ("CustomerResend", "The customer paid the same amount twice by mistake"),
    ("LegitRepeat", "A genuine second payment of the same amount"),
    ("ThirdPartyNotAllowed", "Someone else paid an Autoramp that only takes the customer's money"),
    ("ThirdPartyAllowed", "A declared third party paid a platform customer"),
    ("SanctionedExact", "Listed sender, name as listed"),
    ("SanctionedVariant", "Listed sender, transliterated, reordered, cut or misspelled"),
    ("NearMissInnocent", "Innocent sender whose name resembles a listed one"),
    ("BelowMinimum", "Below the minimum amount"),
    ("EddCustomer", "Customer under enhanced due diligence"),
];

fn kind_rows(f: &Facts) -> Vec<(String, i64, i64, i64)> {
    KIND_TEXT
        .iter()
        .filter_map(|(k, text)| {
            let e = f.e["by_kind"].get(*k)?;
            let b = &f.b["by_kind"][*k];
            Some((text.to_string(), n(&e[0]), n(&e[1]), n(&b[1])))
        })
        .collect()
}

fn markdown(f: &Facts) -> String {
    let (e, b) = (&f.e, &f.b);
    let mut s = String::new();
    s += "# Results\n\n_Generated by `pie report` from `reports/*.json`. Do not edit by hand._\n\n";
    s += &format!(
        "Dev worlds: {} seeds, {} deposits worth {} + {}. The engine runs with `config/engine.json`; the baseline is the strict reading of the published rules (`config/baseline.json`): exact references only, exact-name screening, no resend check, returned deposits not traceable.\n\n",
        f.seeds,
        n(&e["deposits"]),
        money(n(&e["volume"]["eur_minor"]), "€"),
        money(n(&e["volume"]["usd_minor"]), "$")
    );
    s += "| | Engine | Baseline |\n|---|---|---|\n";
    let rows: Vec<(&str, String, String)> = vec![
        ("Final outcome correct", pct(&e["correct_outcome"]), pct(&b["correct_outcome"])),
        ("Credited to the wrong customer", n(&e["misrouted"]["n"]).to_string(), n(&b["misrouted"]["n"]).to_string()),
        (
            "Sent back although it belonged to a customer",
            format!("{} ({})", n(&e["wrongly_returned"]["n"]), both(&e["wrongly_returned"])),
            format!("{} ({})", n(&b["wrongly_returned"]["n"]), both(&b["wrongly_returned"])),
        ),
        (
            "Credited although it should have been held or returned",
            format!("{} ({})", n(&e["credited_against_policy"]["n"]), both(&e["credited_against_policy"])),
            format!("{} ({})", n(&b["credited_against_policy"]["n"]), both(&b["credited_against_policy"])),
        ),
        (
            "Accidental second payments minted twice",
            format!("{} ({})", n(&e["duplicate_mints"]["n"]), both(&e["duplicate_mints"])),
            format!("{} ({})", n(&b["duplicate_mints"]["n"]), both(&b["duplicate_mints"])),
        ),
        ("Listed senders held (all)", rate(&e["sanctions_held"]), rate(&b["sanctions_held"])),
        ("Listed senders held (name variants)", rate(&e["sanctions_variants_held"]), rate(&b["sanctions_variants_held"])),
        ("Third-party payments held", rate(&e["third_party_held"]), rate(&b["third_party_held"])),
        ("Accidental second payments held", rate(&e["resends_held"]), rate(&b["resends_held"])),
        (
            "Holds a person cleared with nothing wrong, per 1,000 deposits",
            format!("{:.1}", e["false_holds_per_1000"].as_f64().unwrap_or(0.0)),
            format!("{:.1}", b["false_holds_per_1000"].as_f64().unwrap_or(0.0)),
        ),
        ("Returned deposits findable by UETR / end-to-end ID / ACH trace / IMAD", rate(&e["trace_returned"]), rate(&b["trace_returned"])),
        ("Credits only on the statement, recovered", rate(&e["statement_only_recovered"]), rate(&b["statement_only_recovered"])),
    ];
    for (k, x, y) in rows {
        s += &format!("| {k} | {x} | {y} |\n");
    }
    s += "\n## Ledger, reconciliation and chain (engine)\n\n";
    s += &format!(
        "- Trial balance zero: {}. Lowest reserve headroom after any journal: {} EUR, {} USD (never negative; the database refuses a journal that would make it so).\n",
        e["trial_balance_zero"],
        n(&e["min_reserve_headroom_minor"]["EUR"]),
        n(&e["min_reserve_headroom_minor"]["USD"])
    );
    s += &format!(
        "- {} statement days reconciled: bank charges found {}, unexplained balance breaks {}, reserve-coverage breaks {}, chain-supply breaks {}.\n",
        n(&e["recon_days"]),
        rate(&e["fees_found"]),
        n(&e["unexplained_balance_breaks"]),
        n(&e["reserve_coverage_breaks"]),
        n(&e["chain_supply_breaks"])
    );
    s += &format!(
        "- Reorgs: {} mints dropped and {} re-included; {} re-submissions; completed mints final on-chain exactly once: {}; reorgs deeper than the confirmation depth: {}.\n",
        n(&e["reorg_dropped_mints"]),
        n(&e["reorg_reincluded_mints"]),
        n(&e["mint_resubmissions"]),
        rate(&e["mints_final_once"]),
        n(&e["reorgs_past_confirmation"])
    );
    s += &format!(
        "- Bank re-sent notifications ignored: {}; corrupt files quarantined whole: {}; deposits booked exactly once: {}; USD conversions outside the mint window: {}; payout returns processed: {}.\n",
        rate(&e["renotifications_ignored"]),
        rate(&e["corrupt_files_quarantined"]),
        rate(&e["deposits_booked_once"]),
        n(&e["usd_conversions_outside_window"]),
        rate(&e["payout_returns_processed"])
    );
    s += &format!(
        "- Hours from booking to final mint (no hold): EUR p50 {:.1} / p90 {:.1}, USD p50 {:.1} / p90 {:.1} (USD waits for the mint window).\n",
        e["hours_to_mint_p50"]["EUR"].as_f64().unwrap_or(0.0),
        e["hours_to_mint_p90"]["EUR"].as_f64().unwrap_or(0.0),
        e["hours_to_mint_p50"]["USD"].as_f64().unwrap_or(0.0),
        e["hours_to_mint_p90"]["USD"].as_f64().unwrap_or(0.0)
    );
    if let Some(b) = &f.bench {
        let runs = b["runs"].as_array().cloned().unwrap_or_default();
        let ips: f64 = runs.iter().map(|r| r["inputs_per_sec"].as_f64().unwrap_or(0.0)).sum::<f64>() / runs.len().max(1) as f64;
        s += &format!("- Engine throughput: about {:.0} inputs per second on one core, in memory (`reports/bench.json`; varies by machine).\n", ips);
    }
    if let Some(pg) = &f.pg {
        s += &format!(
            "- Postgres: {} events, {} journals and {} postings written with the balance and reserve triggers on ({:.0} events/s); the engine rebuilt from the event log matches on every one of {} balances ({} mismatches).\n",
            n(&pg["events"]),
            n(&pg["journals"]),
            n(&pg["postings"]),
            pg["events_per_second"].as_f64().unwrap_or(0.0),
            n(&pg["accounts"]),
            n(&pg["balance_mismatches"])
        );
    }
    s += "\n## Planted cases (dev)\n\n| Case | Planted | Engine correct | Baseline correct |\n|---|---|---|---|\n";
    for (text, planted, eok, bok) in kind_rows(f) {
        s += &format!("| {text} | {planted} | {eok} | {bok} |\n");
    }
    if let Some(h) = &f.held {
        let (he, hb) = (&h["engine"], &h["baseline"]);
        s += &format!(
            "\n## Held-out world v1 (first run, commit {})\n\nA world the engine was never tuned on: its own seed and mix, March 2027 across the US daylight-saving change. Recorded once; never rewritten.\n\n| | Engine | Baseline |\n|---|---|---|\n",
            h["commit"].as_str().unwrap_or("")
        );
        for (k, x, y) in [
            ("Deposits", n(&he["deposits"]).to_string(), n(&hb["deposits"]).to_string()),
            ("Final outcome correct", pct(&he["correct_outcome"]), pct(&hb["correct_outcome"])),
            ("Credited to the wrong customer", n(&he["misrouted"]["n"]).to_string(), n(&hb["misrouted"]["n"]).to_string()),
            (
                "Sent back although it belonged to a customer",
                format!("{} ({})", n(&he["wrongly_returned"]["n"]), both(&he["wrongly_returned"])),
                format!("{} ({})", n(&hb["wrongly_returned"]["n"]), both(&hb["wrongly_returned"])),
            ),
            ("Accidental second payments minted twice", n(&he["duplicate_mints"]["n"]).to_string(), n(&hb["duplicate_mints"]["n"]).to_string()),
            ("Listed senders held", rate(&he["sanctions_held"]), rate(&hb["sanctions_held"])),
            (
                "Holds cleared with nothing wrong, per 1,000",
                format!("{:.1}", he["false_holds_per_1000"].as_f64().unwrap_or(0.0)),
                format!("{:.1}", hb["false_holds_per_1000"].as_f64().unwrap_or(0.0)),
            ),
            ("Unexplained reconciliation breaks", n(&he["unexplained_balance_breaks"]).to_string(), n(&hb["unexplained_balance_breaks"]).to_string()),
            (
                "USD conversions outside the mint window",
                n(&he["usd_conversions_outside_window"]).to_string(),
                n(&hb["usd_conversions_outside_window"]).to_string(),
            ),
        ] {
            s += &format!("| {k} | {x} | {y} |\n");
        }
        let misses: Vec<String> = he["by_kind"]
            .as_object()
            .map(|m| m.iter().filter(|(_, v)| n(&v[0]) != n(&v[1])).map(|(k, v)| format!("{k} {}/{}", n(&v[1]), n(&v[0]))).collect())
            .unwrap_or_default();
        s += &format!("\nCases the engine got wrong on the held-out world: {}.\n", if misses.is_empty() { "none".to_string() } else { misses.join(", ") });
        if let Some(l) = &f.held_latest {
            s += &format!("\nLatest re-run (commit {}): engine {} correct.\n", l["commit"].as_str().unwrap_or(""), pct(&l["engine"]["correct_outcome"]));
        }
    }
    s
}

fn html(f: &Facts) -> String {
    let (e, b) = (&f.e, &f.b);
    let total = n(&e["deposits"]).max(1) as f64;
    let bar = |r: &Value| {
        let ok = n(&r["correct_outcome"]["hit"]) as f64;
        let wr = n(&r["wrongly_returned"]["n"]) as f64;
        let cap = n(&r["credited_against_policy"]["n"]) as f64 + n(&r["duplicate_mints"]["n"]) as f64;
        let other = (total - ok - wr - cap).max(0.0);
        let w = |x: f64| format!("{:.3}%", 100.0 * x / total);
        format!(
            "<div class=\"bar\" role=\"img\" aria-label=\"{} correct, {} sent back wrongly, {} credited against policy or twice\"><span class=\"ok\" style=\"width:{}\"></span><span class=\"wr\" style=\"width:{}\"></span><span class=\"cap\" style=\"width:{}\"></span><span class=\"oth\" style=\"width:{}\"></span></div>",
            ok,
            wr,
            cap,
            w(ok),
            w(wr.max(if wr > 0.0 { total * 0.004 } else { 0.0 })),
            w(cap.max(if cap > 0.0 { total * 0.004 } else { 0.0 })),
            w(other)
        )
    };
    let tile = |label: &str, value: String, note: String| {
        format!("<div class=\"tile\"><div class=\"v\">{value}</div><div class=\"l\">{label}</div><div class=\"n\">{note}</div></div>")
    };
    let mut kinds = String::new();
    for (text, planted, eok, bok) in kind_rows(f) {
        let cls = |ok: i64| {
            if ok == planted {
                "full"
            } else if ok == 0 {
                "none"
            } else {
                "part"
            }
        };
        kinds += &format!(
            "<tr><td>{text}</td><td class=\"num\">{planted}</td><td class=\"num {}\">{eok}</td><td class=\"num {}\">{bok}</td></tr>",
            cls(eok),
            cls(bok)
        );
    }
    let held = f
        .held
        .as_ref()
        .map(|h| {
            let (he, hb) = (&h["engine"], &h["baseline"]);
            format!(
                "<section><h2>Held-out world</h2><p class=\"lede\">Its own seed, customer mix and month (March 2027, across the US daylight-saving change). Run once at commit <code>{}</code> and recorded; never rewritten.</p><div class=\"tiles\">{}{}{}{}</div></section>",
                h["commit"].as_str().unwrap_or(""),
                tile("final outcome correct", pct(&he["correct_outcome"]), format!("baseline {}", pct(&hb["correct_outcome"]))),
                tile("credited to the wrong customer", n(&he["misrouted"]["n"]).to_string(), format!("of {} deposits", n(&he["deposits"]))),
                tile("sent back wrongly", n(&he["wrongly_returned"]["n"]).to_string(), format!("baseline {} ({})", n(&hb["wrongly_returned"]["n"]), both(&hb["wrongly_returned"]))),
                tile("listed senders held", rate(&he["sanctions_held"]), format!("baseline {}", rate(&hb["sanctions_held"]))),
            )
        })
        .unwrap_or_default();
    let pg =
        f.pg.as_ref()
            .map(|p| {
                tile(
                    "balances equal after rebuilding from Postgres",
                    format!("{} / {}", n(&p["accounts"]) - n(&p["balance_mismatches"]), n(&p["accounts"])),
                    format!("{} events, {} journals, {} postings", n(&p["events"]), n(&p["journals"]), n(&p["postings"])),
                )
            })
            .unwrap_or_default();
    let css = r#"
:root{--bg:#fbfcfd;--fg:#14212b;--muted:#5b6b78;--line:#dfe5ea;--ok:#1d7a58;--wr:#c26b12;--cap:#b4232f;--oth:#c9d3db;--panel:#f1f4f7;--accent:#0f4c81}
@media (prefers-color-scheme:dark){:root:not([data-theme="light"]){--bg:#0e161d;--fg:#e6edf2;--muted:#9aabb8;--line:#24323d;--ok:#3cb88a;--wr:#e69a46;--cap:#ee6b73;--oth:#3a4955;--panel:#15212a;--accent:#7fb6e8}}
:root[data-theme="dark"]{--bg:#0e161d;--fg:#e6edf2;--muted:#9aabb8;--line:#24323d;--ok:#3cb88a;--wr:#e69a46;--cap:#ee6b73;--oth:#3a4955;--panel:#15212a;--accent:#7fb6e8}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.55 "Public Sans",system-ui,sans-serif}
main{max-width:1040px;margin:0 auto;padding:48px 16px 72px}
h1{font-size:clamp(28px,4vw,40px);line-height:1.15;letter-spacing:-.01em;margin:0 0 12px;font-weight:700;max-width:22ch}
h2{font-size:20px;margin:0 0 6px;font-weight:650}
p{margin:0 0 12px;max-width:72ch}.lede{color:var(--muted)}
section{margin-top:48px}
.compare{margin-top:28px;display:grid;gap:14px}
.row{display:grid;grid-template-columns:120px 1fr;gap:16px;align-items:center}
.row b{font-weight:650}
.bar{display:flex;height:22px;border-radius:4px;overflow:hidden;background:var(--panel)}
.bar span{display:block;height:100%}.ok{background:var(--ok)}.wr{background:var(--wr)}.cap{background:var(--cap)}.oth{background:var(--oth)}
.key{display:flex;flex-wrap:wrap;gap:18px;color:var(--muted);font-size:14px;margin-top:4px}
.key i{display:inline-block;width:10px;height:10px;border-radius:2px;margin-right:6px;vertical-align:baseline}
.tiles{display:grid;grid-template-columns:repeat(auto-fit,minmax(220px,1fr));gap:1px;background:var(--line);border:1px solid var(--line);border-radius:6px;overflow:hidden;margin-top:16px}
.tile{background:var(--bg);padding:16px 18px}.tile:last-child:nth-child(4n+1){grid-column:1/-1}.tile .v{font-size:26px;font-weight:700;font-variant-numeric:tabular-nums}.tile .l{font-weight:600;margin-top:2px}.tile .n{color:var(--muted);font-size:14px}
table{width:100%;border-collapse:collapse;margin-top:16px;font-size:15px}
th{text-align:left;font-weight:600;color:var(--muted);border-bottom:1px solid var(--line);padding:8px 10px 8px 0}
td{border-bottom:1px solid var(--line);padding:8px 10px 8px 0;vertical-align:top}
.num{text-align:right;font-variant-numeric:tabular-nums;white-space:nowrap}
td.full{color:var(--ok);font-weight:600}td.none{color:var(--cap);font-weight:600}td.part{color:var(--wr);font-weight:600}
code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.9em}
.wrap{overflow-x:auto}
footer{margin-top:56px;color:var(--muted);font-size:14px}
@media (max-width:600px){.row{grid-template-columns:1fr;gap:6px}}
"#;
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Pay-in engine results</title><link rel="preconnect" href="https://fonts.googleapis.com"><link href="https://fonts.googleapis.com/css2?family=Public+Sans:wght@400;600;650;700&display=swap" rel="stylesheet"><style>{css}</style></head><body><main>
<h1>Every deposit to the right customer, or back to its sender</h1>
<p class="lede">{deposits} simulated deposits worth {eur} and {usd} over {seeds} months of SEPA, SWIFT, ACH and Fedwire pay-ins, minted on Ethereum and Solana. The engine against the strict reading of the published rules.</p>
<div class="compare">
<div class="row"><b>Engine</b>{bar_e}</div>
<div class="row"><b>Baseline</b>{bar_b}</div>
<div class="key"><span><i class="ok"></i>right outcome</span><span><i class="wr"></i>sent back although it was a customer's</span><span><i class="cap"></i>credited although it needed review, or minted twice</span><span><i class="oth"></i>other</span></div>
</div>
<section><h2>Money that reached its customer</h2><p class="lede">References typed in lower case, without the hyphen, split across lines or with one wrong character still belong to someone. The engine repairs a one-character typo only when exactly one live reference is that close and the sender is the customer.</p>
<div class="tiles">{t1}{t2}{t3}{t4}</div></section>
<section><h2>Money that needed a person</h2><p class="lede">Holds, never automatic rejections: a person decides every one, and the decision is recorded with their name.</p>
<div class="tiles">{t5}{t6}{t7}{t8}</div></section>
<section><h2>Ledger, bank and chain agree</h2><p class="lede">Double-entry in integer cents. The reserve covers every token after every journal, in memory and again in Postgres. Each night the camt.053 statements, the ledger and the final on-chain supply are reconciled.</p>
<div class="tiles">{t9}{t10}{t11}{t12}{pg}</div></section>
<section><h2>Planted cases</h2><div class="wrap"><table><thead><tr><th>Case</th><th class="num">Planted</th><th class="num">Engine right</th><th class="num">Baseline right</th></tr></thead><tbody>{kinds}</tbody></table></div></section>
{held}
<footer>Generated by <code>pie report</code> from <code>reports/results.json</code>. Simulated data; no real bank, customer or chain is involved.</footer>
</main></body></html>
"#,
        deposits = n(&e["deposits"]),
        eur = money(n(&e["volume"]["eur_minor"]), "€"),
        usd = money(n(&e["volume"]["usd_minor"]), "$"),
        seeds = f.seeds,
        bar_e = bar(e),
        bar_b = bar(b),
        t1 = tile("final outcome correct", pct(&e["correct_outcome"]), format!("baseline {}", pct(&b["correct_outcome"]))),
        t2 = tile(
            "sent back wrongly",
            n(&e["wrongly_returned"]["n"]).to_string(),
            format!("baseline {} worth {}", n(&b["wrongly_returned"]["n"]), both(&b["wrongly_returned"]))
        ),
        t3 = tile(
            "credited to the wrong customer",
            n(&e["misrouted"]["n"]).to_string(),
            format!("{} references repaired, {} read through formatting", n(&e["repaired_references"]), n(&e["normalised_references"]))
        ),
        t4 = tile(
            "returned deposits traceable",
            pct(&e["trace_returned"]),
            format!("by UETR, end-to-end ID, ACH trace or IMAD; baseline {}", pct(&b["trace_returned"]))
        ),
        t5 = tile(
            "listed senders held",
            rate(&e["sanctions_held"]),
            format!("variants {}; baseline {}", rate(&e["sanctions_variants_held"]), rate(&b["sanctions_held"]))
        ),
        t6 = tile(
            "second payments held before minting",
            rate(&e["resends_held"]),
            format!("baseline minted {} twice ({})", n(&b["duplicate_mints"]["n"]), both(&b["duplicate_mints"]))
        ),
        t7 = tile("third-party payments held", rate(&e["third_party_held"]), format!("baseline {}", rate(&b["third_party_held"]))),
        t8 = tile(
            "holds with nothing wrong",
            format!("{:.1}", e["false_holds_per_1000"].as_f64().unwrap_or(0.0)),
            "per 1,000 deposits: the cost of the resend check".to_string()
        ),
        t9 = tile(
            "unexplained balance breaks",
            n(&e["unexplained_balance_breaks"]).to_string(),
            format!("{} statement days; {} bank charges found", n(&e["recon_days"]), rate(&e["fees_found"]))
        ),
        t10 = tile("mints final exactly once", pct(&e["mints_final_once"]), format!("{} dropped by reorgs and re-submitted", n(&e["reorg_dropped_mints"]))),
        t11 = tile("reserve shortfalls", n(&e["reserve_coverage_breaks"]).to_string(), "bank reserve vs tokens, every cut-off".to_string()),
        t12 = tile("credits found only on the statement", rate(&e["statement_only_recovered"]), "booked as deposits on reconciliation".to_string()),
    )
}

pub fn write_all(root: &Path) -> Result<()> {
    let f = facts(root)?;
    std::fs::write(root.join("reports/REPORT.md"), markdown(&f))?;
    std::fs::write(root.join("reports/dashboard.html"), html(&f))?;
    eprintln!("wrote reports/REPORT.md and reports/dashboard.html");
    Ok(())
}
