//! HTTP API. Every POST needs an `Idempotency-Key` and follows MoonPay Enterprise's published rules:
//! same key and same request replays the stored 2xx, same key and a different request is 409, a failed
//! request stores nothing. Decisions on held deposits need a named person.

use crate::engine::{Decision, Engine, Input};
use crate::money::format_decimal;
use crate::routing::Route;
use crate::store::{Idem, PgStore, sha256_hex};
use crate::time::{Ts, fmt_iso};
use anyhow::Result;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct Service {
    pub engine: Engine,
    pub store: PgStore,
    cfg: crate::config::EngineConfig,
    platform: crate::domain::Platform,
}

pub struct AppState {
    pub service: Mutex<Service>,
    /// A separate connection for idempotency keys, so a second request with the same key sees the
    /// first one in flight instead of queueing behind it.
    pub idem: PgStore,
}

pub type Shared = Arc<AppState>;

pub fn now() -> Ts {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Service {
    /// Connect, migrate, and rebuild the engine from the event log.
    pub async fn start(url: &str, cfg: crate::config::EngineConfig, platform: crate::domain::Platform) -> Result<Service> {
        let store = PgStore::connect(url).await?;
        store.migrate().await?;
        let mut s = Service { engine: Engine::new(cfg.clone(), platform.clone()), store, cfg, platform };
        s.rebuild().await?;
        Ok(s)
    }

    pub async fn rebuild(&mut self) -> Result<()> {
        let mut e = Engine::new(self.cfg.clone(), self.platform.clone());
        for (at, input) in self.store.events().await? {
            e.handle(at, input)?;
            e.drain();
        }
        self.engine = e;
        Ok(())
    }

    /// Apply an input and record it with its journals. Returns false for an input already recorded.
    pub async fn apply(&mut self, at: Ts, input: Input) -> Result<bool> {
        if self.store.seen(&input).await? {
            return Ok(false);
        }
        let before = self.engine.state.ledger.journals.len();
        self.engine.handle(at, input.clone())?;
        self.engine.drain();
        let journals = self.engine.state.ledger.journals[before..].to_vec();
        if let Err(e) = self.store.record(at, &input, &journals).await {
            // The database is the record: put memory back in line with it before reporting.
            self.rebuild().await?;
            return Err(e);
        }
        Ok(true)
    }
}

pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/api/bank-files", post(post_bank_file))
        .route("/api/holds", get(list_holds))
        .route("/api/holds/{id}/decision", post(post_decision))
        .route("/api/trace/{id}", get(trace))
        .route("/api/autoramp-transactions/{id}", get(get_transaction))
        .route("/api/ledger/balances", get(balances))
        .route("/api/reconciliation/latest", get(latest_recon))
        .with_state(state)
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({"error": msg}))).into_response()
}

/// Run `f` under the idempotency rules for `key`.
async fn idempotent<F, Fut>(state: &Shared, headers: &HeaderMap, path: &str, body: &[u8], f: F) -> Response
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = (StatusCode, Value)>,
{
    let Some(key) = headers.get("idempotency-key").and_then(|v| v.to_str().ok()).map(str::to_string) else {
        return err(StatusCode::BAD_REQUEST, "missing Idempotency-Key header");
    };
    let fp = sha256_hex(&[path.as_bytes(), b"\n", body].concat());
    match state.idem.begin_idempotent(&key, &fp).await {
        Ok(Idem::New) => {}
        Ok(Idem::Replay(code, v)) => return (StatusCode::from_u16(code).unwrap_or(StatusCode::OK), Json(v)).into_response(),
        Ok(Idem::Conflict) => return StatusCode::CONFLICT.into_response(),
        Ok(Idem::InFlight) => return err(StatusCode::CONFLICT, "a request with this Idempotency-Key is still in flight"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
    let (code, v) = f().await;
    if code.is_success() {
        let _ = state.idem.finish_idempotent(&key, code.as_u16(), &v).await;
    } else {
        let _ = state.idem.abandon_idempotent(&key).await;
    }
    (code, Json(v)).into_response()
}

fn deposit_json(e: &Engine, d: &crate::engine::Deposit) -> Value {
    let tx = d.transaction_id.as_ref().and_then(|t| e.transaction(t));
    let hold = tx.and_then(|t| t.hold_id.as_ref()).and_then(|h| e.hold(h));
    json!({
        "deposit_id": d.id,
        "rail": d.payment.rail.name(),
        "amount": format_decimal(d.payment.amount_minor, d.payment.ccy),
        "currency": d.payment.ccy.code(),
        "sender": d.payment.debtor_name,
        "booked_at": fmt_iso(d.payment.booked_at),
        "ids": d.payment.ids,
        "source": d.source,
        "route": match &d.route {
            Route::Matched { autoramp_id, how } => json!({"matched": autoramp_id, "how": how}),
            Route::Return { reason } => json!({"returned": reason, "why": reason.text()}),
        },
        "returned": d.returned.as_ref().map(|(at, why, id)| json!({"at": fmt_iso(*at), "why": why, "return_id": id})),
        "transaction": tx.map(|t| json!({
            "id": t.id,
            "status": t.status,
            "history": t.history.iter().map(|(at, s, n)| json!({"at": fmt_iso(*at), "status": s, "note": n})).collect::<Vec<_>>(),
            "mint": t.mint,
        })),
        "hold": hold.map(|h| json!({"id": h.id, "reasons": h.reasons, "decided": h.decided})),
    })
}

async fn post_bank_file(State(state): State<Shared>, headers: HeaderMap, body: String) -> Response {
    let name = headers.get("x-file-name").and_then(|v| v.to_str().ok()).unwrap_or("upload").to_string();
    let st = state.clone();
    let fingerprint_body = body.clone();
    idempotent(&state, &headers, "/api/bank-files", fingerprint_body.as_bytes(), || async move {
        let mut s = st.service.lock().await;
        let before = s.engine.state.deposits.len();
        let quarantined_before = s.engine.state.quarantined.len();
        match s.apply(now(), Input::BankFile { name, raw: body }).await {
            Ok(recorded) => {
                let e = &s.engine;
                let deposits: Vec<Value> = e.state.deposits[before..].iter().map(|d| deposit_json(e, d)).collect();
                let quarantined = e.state.quarantined.get(quarantined_before).map(|q| q.error.clone());
                (StatusCode::CREATED, json!({"recorded": recorded, "deposits": deposits, "quarantined": quarantined}))
            }
            Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, json!({"error": format!("{e:#}")})),
        }
    })
    .await
}

#[derive(Deserialize)]
struct DecisionBody {
    decision: String,
    by: String,
}

async fn post_decision(State(state): State<Shared>, Path(id): Path<String>, headers: HeaderMap, body: String) -> Response {
    let st = state.clone();
    let path = format!("/api/holds/{id}/decision");
    let fingerprint_body = body.clone();
    idempotent(&state, &headers, &path, fingerprint_body.as_bytes(), || async move {
        let Ok(b) = serde_json::from_str::<DecisionBody>(&body) else {
            return (StatusCode::UNPROCESSABLE_ENTITY, json!({"error": "body must be {\"decision\": ..., \"by\": ...}"}));
        };
        if b.by.trim().is_empty() {
            return (StatusCode::UNPROCESSABLE_ENTITY, json!({"error": "a decision needs the name of the person making it"}));
        }
        let decision = match b.decision.as_str() {
            "release" => Decision::Release,
            "reject" => Decision::Reject,
            "return" => Decision::Return,
            _ => return (StatusCode::UNPROCESSABLE_ENTITY, json!({"error": "decision must be release, reject or return"})),
        };
        let mut s = st.service.lock().await;
        let Some(h) = s.engine.hold(&id).cloned() else { return (StatusCode::NOT_FOUND, json!({"error": "no such hold"})) };
        if let Some((_, d, _)) = h.decided {
            if d != decision {
                return (StatusCode::CONFLICT, json!({"error": format!("already decided: {d:?}")}));
            }
        }
        match s.apply(now(), Input::Decision { hold_id: id.clone(), decision, by: b.by }).await {
            Ok(_) => {
                let t = s.engine.transaction(&h.transaction_id).cloned();
                (StatusCode::OK, json!({"hold": id, "decision": decision, "transaction": t.map(|t| json!({"id": t.id, "status": t.status}))}))
            }
            Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, json!({"error": format!("{e:#}")})),
        }
    })
    .await
}

#[derive(Deserialize)]
struct HoldsQuery {
    open: Option<bool>,
}

async fn list_holds(State(state): State<Shared>, Query(q): Query<HoldsQuery>) -> Response {
    let s = state.service.lock().await;
    let open_only = q.open.unwrap_or(true);
    let v: Vec<Value> = s
        .engine
        .state
        .holds
        .iter()
        .filter(|h| !open_only || h.decided.is_none())
        .map(|h| json!({"id": h.id, "transaction_id": h.transaction_id, "reasons": h.reasons, "opened_at": fmt_iso(h.opened_at), "decided": h.decided}))
        .collect();
    Json(json!({"holds": v})).into_response()
}

async fn trace(State(state): State<Shared>, Path(id): Path<String>) -> Response {
    let s = state.service.lock().await;
    match s.engine.trace(&id) {
        Some(d) => Json(deposit_json(&s.engine, d)).into_response(),
        None => err(StatusCode::NOT_FOUND, "no deposit with that UETR, end-to-end ID, bank reference, ACH trace number or IMAD"),
    }
}

async fn get_transaction(State(state): State<Shared>, Path(id): Path<String>) -> Response {
    let s = state.service.lock().await;
    match s.engine.transaction(&id) {
        Some(t) => Json(json!(t)).into_response(),
        None => err(StatusCode::NOT_FOUND, "no such transaction"),
    }
}

async fn balances(State(state): State<Shared>) -> Response {
    let s = state.service.lock().await;
    match s.store.balances().await {
        Ok(b) => Json(json!(b.into_iter().map(|((a, c), v)| json!({"account": a, "ccy": c, "balance_minor": v})).collect::<Vec<_>>())).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn latest_recon(State(state): State<Shared>) -> Response {
    let s = state.service.lock().await;
    match s.engine.state.recon.last() {
        Some(r) => Json(json!(r)).into_response(),
        None => err(StatusCode::NOT_FOUND, "no statement reconciled yet"),
    }
}
