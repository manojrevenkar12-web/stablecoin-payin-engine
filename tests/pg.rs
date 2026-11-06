//! Postgres and the HTTP API. Needs a database: set PIE_DATABASE_URL (CI starts one); skipped otherwise.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use stablecoin_payin_engine::api::{self, AppState, Service};
use stablecoin_payin_engine::domain::{InboundPayment, Rail, TraceIds};
use stablecoin_payin_engine::money::Ccy;
use stablecoin_payin_engine::rails::iso::write_camt054;
use stablecoin_payin_engine::store::PgStore;
use stablecoin_payin_engine::{config, sim};
use std::sync::Arc;
use tower::ServiceExt;

fn url() -> Option<String> {
    std::env::var("PIE_DATABASE_URL").ok()
}

async fn call(app: &axum::Router, method: &str, path: &str, key: Option<&str>, body: String) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path).header("x-file-name", "test.xml");
    if let Some(k) = key {
        req = req.header("idempotency-key", k);
    }
    let resp = app.clone().oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn credit(platform: &stablecoin_payin_engine::domain::Platform, sender: &str, e2e: &str, svcr: &str, amount: i64) -> InboundPayment {
    let ar = platform
        .autoramps
        .iter()
        .find(|a| {
            a.ccy == Ccy::EUR
                && !a.pay_by_reference
                && a.status == stablecoin_payin_engine::domain::AutorampStatus::Approved
                && platform.customer(&a.customer_id).unwrap().status == stablecoin_payin_engine::domain::CustomerStatus::Active
        })
        .unwrap();
    let sender = if sender.is_empty() { platform.customer(&ar.customer_id).unwrap().legal_name.clone() } else { sender.to_string() };
    InboundPayment {
        rail: Rail::Sepa,
        bank_ref: svcr.into(),
        ids: TraceIds { acct_svcr_ref: Some(svcr.into()), end_to_end_id: Some(e2e.into()), ..Default::default() },
        amount_minor: amount,
        ccy: Ccy::EUR,
        booked_at: api::now() - 60,
        debtor_name: sender,
        debtor_account: "DE89370400440532013000".into(),
        creditor_account: ar.deposit_account.clone(),
        structured_ref: None,
        unstructured: Some("Invoice 1".into()),
    }
}

#[tokio::test]
async fn postgres_is_the_record_and_the_api_is_idempotent() {
    let Some(url) = url() else {
        eprintln!("PIE_DATABASE_URL not set: skipping the Postgres tests");
        return;
    };
    let mut pg = PgStore::connect(&url).await.unwrap();
    pg.reset().await.unwrap();

    // The database refuses an unbalanced journal, a mint without reserve, and edits to the record.
    let tx = pg.client.transaction().await.unwrap();
    tx.execute("insert into events (at, kind, body) values (0, 'test', '{}')", &[]).await.unwrap();
    tx.execute("insert into journals (id, at, kind, reference, event_seq) values (1, 0, 'x', 'x', (select max(seq) from events))", &[]).await.unwrap();
    tx.execute("insert into postings values (1, 'bank:EUR:operating', 'EUR', 100), (1, 'customer:c:EUR:held', 'EUR', -99)", &[]).await.unwrap();
    let e = tx.commit().await.unwrap_err();
    assert!(format!("{e:?}").contains("does not balance"), "{e:?}");
    let tx = pg.client.transaction().await.unwrap();
    tx.execute("insert into events (at, kind, body) values (0, 'test', '{}')", &[]).await.unwrap();
    tx.execute("insert into journals (id, at, kind, reference, event_seq) values (2, 0, 'mint', 'x', (select max(seq) from events))", &[]).await.unwrap();
    tx.execute("insert into postings values (2, 'customer:c:EUR:held', 'EUR', 500), (2, 'tokens:EUR:pending_mint', 'EUR', -500)", &[]).await.unwrap();
    let e = tx.commit().await.unwrap_err();
    assert!(format!("{:?}", e).contains("reserve"), "{e:?}");
    pg.client.execute("insert into events (at, kind, body) values (0, 'test', '{}')", &[]).await.unwrap();
    assert!(pg.client.execute("update events set at = 1", &[]).await.is_err(), "events are append-only");
    pg.reset().await.unwrap();

    // The API over a fresh database.
    let w = sim::world::generate(&sim::world::WorldCfg::load("world").unwrap());
    let cfg = config::load_named("engine").unwrap();
    let service = Service::start(&url, cfg.clone(), w.platform.clone()).await.unwrap();
    let idem = PgStore::connect(&url).await.unwrap();
    let app = api::router(Arc::new(AppState { service: tokio::sync::Mutex::new(service), idem }));

    let acct = w.platform.operating_account(Ccy::EUR).unwrap().to_string();
    let p = credit(&w.platform, "", "E2E-TEST-1", "SVCR-1", 1_234_56);
    let file = write_camt054("T1", p.booked_at, &acct, Ccy::EUR, std::slice::from_ref(&p));

    let (s, _) = call(&app, "POST", "/api/bank-files", None, file.clone()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "Idempotency-Key is required");
    let (s, first) = call(&app, "POST", "/api/bank-files", Some("k-1"), file.clone()).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(first["deposits"].as_array().unwrap().len(), 1);
    assert!(first["deposits"][0]["route"]["matched"].is_string(), "{first}");
    let (s, replay) = call(&app, "POST", "/api/bank-files", Some("k-1"), file.clone()).await;
    assert_eq!((s, &replay), (StatusCode::CREATED, &first), "same key, same request: the stored response");
    let (s, _) = call(&app, "POST", "/api/bank-files", Some("k-1"), file.replace("Invoice 1", "Invoice 2")).await;
    assert_eq!(s, StatusCode::CONFLICT, "same key, different request");
    let (s, again) = call(&app, "POST", "/api/bank-files", Some("k-2"), file.clone()).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(again["recorded"], false, "the same file under a new key is recorded once");

    // Trace by the sender's end-to-end ID.
    let (s, t) = call(&app, "GET", "/api/trace/e2e-test-1", None, String::new()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(t["amount"], "1234.56");
    let (s, _) = call(&app, "GET", "/api/trace/nope", None, String::new()).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // A listed sender is held; a decision needs a person's name.
    let listed = w.platform.sanctions[0].names[0].clone();
    let q = credit(&w.platform, &listed, "E2E-TEST-2", "SVCR-2", 50_000_00);
    let (s, held) = call(&app, "POST", "/api/bank-files", Some("k-3"), write_camt054("T2", q.booked_at, &acct, Ccy::EUR, &[q])).await;
    assert_eq!(s, StatusCode::CREATED);
    let hold = held["deposits"][0]["hold"]["id"].as_str().unwrap().to_string();
    let path = format!("/api/holds/{hold}/decision");
    let (s, _) = call(&app, "POST", &path, Some("d-1"), r#"{"decision":"reject","by":""}"#.into()).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, d) = call(&app, "POST", &path, Some("d-2"), r#"{"decision":"reject","by":"A. Analyst"}"#.into()).await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert_eq!(d["transaction"]["status"], "RejectedAml");
    let (s, d) = call(&app, "POST", &path, Some("d-3"), r#"{"decision":"release","by":"Someone Else"}"#.into()).await;
    assert_eq!(s, StatusCode::CONFLICT, "{d}");

    // Two requests with one key at the same time: one runs, the other is told it is in flight (or
    // gets the replay); the file is booked once.
    let r = credit(&w.platform, "", "E2E-TEST-3", "SVCR-3", 77_00);
    let f3 = write_camt054("T3", r.booked_at, &acct, Ccy::EUR, &[r]);
    let (a, b) = tokio::join!(call(&app, "POST", "/api/bank-files", Some("k-4"), f3.clone()), call(&app, "POST", "/api/bank-files", Some("k-4"), f3.clone()));
    let codes = [a.0, b.0];
    assert!(codes.contains(&StatusCode::CREATED), "{codes:?}");
    assert!(codes.iter().all(|c| *c == StatusCode::CREATED || *c == StatusCode::CONFLICT), "{codes:?}");

    // Restart: memory rebuilt from the event log agrees with the database.
    let rebuilt = Service::start(&url, cfg, w.platform.clone()).await.unwrap();
    assert_eq!(rebuilt.engine.state.deposits.len(), 3);
    assert!(rebuilt.engine.trace("E2E-TEST-3").is_some());
    let db = rebuilt.store.balances().await.unwrap();
    for ((a, c), v) in rebuilt.engine.state.ledger.balances() {
        assert_eq!(db.get(&(a.clone(), c.code().to_string())), Some(v), "{a}");
    }
}
