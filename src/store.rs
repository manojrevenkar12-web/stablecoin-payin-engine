//! Postgres as the record: an append-only event log, the journals each event produced (written in the
//! same transaction), and API idempotency keys. On start the engine is rebuilt by replaying events.

use crate::engine::Input;
use crate::ledger::Journal;
use crate::time::Ts;
use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use tokio_postgres::{Client, NoTls};

pub const SCHEMA: &str = include_str!("../migrations/001_schema.sql");

pub fn sha256_hex(b: &[u8]) -> String {
    let d = Sha256::digest(b);
    d.iter().map(|x| format!("{x:02x}")).collect()
}

/// The key that makes an input recorded once: the same bank file delivered twice, the same decision
/// submitted twice, the same block seen twice.
pub fn event_key(input: &Input) -> Option<String> {
    match input {
        Input::BankFile { raw, .. } => Some(format!("file:{}", sha256_hex(raw.as_bytes()))),
        Input::Decision { hold_id, .. } => Some(format!("decision:{hold_id}")),
        Input::ChainBlock(b) => Some(format!("block:{}:{}", b.chain, b.hash)),
        Input::Tick => None,
    }
}

fn kind_of(input: &Input) -> &'static str {
    match input {
        Input::BankFile { .. } => "bank_file",
        Input::ChainBlock(_) => "chain_block",
        Input::Decision { .. } => "decision",
        Input::Tick => "tick",
    }
}

pub enum Idem {
    /// First time: go ahead, then `finish` or `abandon`.
    New,
    /// Seen with the same fingerprint: replay this status and body.
    Replay(u16, serde_json::Value),
    /// Same key, different request.
    Conflict,
    /// Same key, a request is still running.
    InFlight,
}

pub struct PgStore {
    pub client: Client,
}

impl PgStore {
    pub async fn connect(url: &str) -> Result<PgStore> {
        let (client, conn) = tokio_postgres::connect(url, NoTls).await.with_context(|| "connecting to Postgres")?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                eprintln!("postgres connection: {e}");
            }
        });
        Ok(PgStore { client })
    }

    pub async fn migrate(&self) -> Result<()> {
        self.client.batch_execute(SCHEMA).await.context("applying the schema")?;
        Ok(())
    }

    /// Test helper: an empty database.
    pub async fn reset(&self) -> Result<()> {
        self.client.batch_execute("drop view if exists balances; drop table if exists postings, journals, events, idempotency_keys cascade;").await?;
        self.migrate().await
    }

    /// Has this input been recorded already?
    pub async fn seen(&self, input: &Input) -> Result<bool> {
        let Some(k) = event_key(input) else { return Ok(false) };
        Ok(self.client.query_opt("select 1 from events where idempotency_key = $1", &[&k]).await?.is_some())
    }

    /// Record an input and the journals it produced, atomically. Returns false if it was already there.
    pub async fn record(&mut self, at: Ts, input: &Input, journals: &[Journal]) -> Result<bool> {
        let key = event_key(input);
        let body = serde_json::to_value(input)?;
        let tx = self.client.transaction().await?;
        let row = tx
            .query_opt(
                "insert into events (at, kind, idempotency_key, body) values ($1, $2, $3, $4) on conflict (idempotency_key) do nothing returning seq",
                &[&at, &kind_of(input), &key, &body],
            )
            .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Ok(false);
        };
        let seq: i64 = row.get(0);
        for j in journals {
            tx.execute(
                "insert into journals (id, at, kind, reference, event_seq) values ($1, $2, $3, $4, $5)",
                &[&(j.id as i64), &j.at, &j.kind, &j.reference, &seq],
            )
            .await?;
            for p in &j.postings {
                tx.execute(
                    "insert into postings (journal_id, account, ccy, amount_minor) values ($1, $2, $3, $4)",
                    &[&(j.id as i64), &p.account, &p.ccy.code(), &p.amount_minor],
                )
                .await?;
            }
        }
        tx.commit()
            .await
            .map_err(|e| anyhow!("the database refused the journals: {}", e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())))?;
        Ok(true)
    }

    /// Bulk variant for replays: many inputs in one transaction.
    pub async fn record_batch(&mut self, items: &[(Ts, Input, Vec<Journal>)]) -> Result<()> {
        let tx = self.client.transaction().await?;
        let ins_event = tx.prepare("insert into events (at, kind, idempotency_key, body) values ($1, $2, $3, $4) returning seq").await?;
        let ins_j = tx.prepare("insert into journals (id, at, kind, reference, event_seq) values ($1, $2, $3, $4, $5)").await?;
        let ins_p = tx.prepare("insert into postings (journal_id, account, ccy, amount_minor) values ($1, $2, $3, $4)").await?;
        for (at, input, journals) in items {
            let key = event_key(input);
            let body = serde_json::to_value(input)?;
            let seq: i64 = tx.query_one(&ins_event, &[at, &kind_of(input), &key, &body]).await?.get(0);
            for j in journals {
                tx.execute(&ins_j, &[&(j.id as i64), &j.at, &j.kind, &j.reference, &seq]).await?;
                for p in &j.postings {
                    tx.execute(&ins_p, &[&(j.id as i64), &p.account, &p.ccy.code(), &p.amount_minor]).await?;
                }
            }
        }
        tx.commit()
            .await
            .map_err(|e| anyhow!("the database refused the batch: {}", e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())))?;
        Ok(())
    }

    pub async fn events(&self) -> Result<Vec<(Ts, Input)>> {
        let rows = self.client.query("select at, body from events order by seq", &[]).await?;
        rows.into_iter().map(|r| Ok((r.get::<_, i64>(0), serde_json::from_value(r.get::<_, serde_json::Value>(1))?))).collect()
    }

    pub async fn balances(&self) -> Result<BTreeMap<(String, String), i64>> {
        let rows = self.client.query("select account, ccy, balance_minor from balances", &[]).await?;
        Ok(rows.into_iter().map(|r| ((r.get::<_, String>(0), r.get::<_, String>(1).trim().to_string()), r.get::<_, i64>(2))).collect())
    }

    pub async fn begin_idempotent(&self, key: &str, fingerprint: &str) -> Result<Idem> {
        let inserted = self
            .client
            .execute("insert into idempotency_keys (key, fingerprint, state) values ($1, $2, 'in_flight') on conflict (key) do nothing", &[&key, &fingerprint])
            .await?;
        if inserted == 1 {
            return Ok(Idem::New);
        }
        let row = self.client.query_one("select fingerprint, state, status_code, body from idempotency_keys where key = $1", &[&key]).await?;
        let fp: String = row.get(0);
        let state: String = row.get(1);
        if fp != fingerprint {
            return Ok(Idem::Conflict);
        }
        if state == "in_flight" {
            return Ok(Idem::InFlight);
        }
        Ok(Idem::Replay(row.get::<_, i32>(2) as u16, row.get(3)))
    }

    pub async fn finish_idempotent(&self, key: &str, status: u16, body: &serde_json::Value) -> Result<()> {
        self.client.execute("update idempotency_keys set state = 'done', status_code = $2, body = $3 where key = $1", &[&key, &(status as i32), body]).await?;
        Ok(())
    }

    /// A failed request stores nothing, so a retry with the same key runs again.
    pub async fn abandon_idempotent(&self, key: &str) -> Result<()> {
        self.client.execute("delete from idempotency_keys where key = $1 and state = 'in_flight'", &[&key]).await?;
        Ok(())
    }
}
