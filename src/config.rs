//! Engine settings. Every threshold the engine acts on is read from `config/*.json`; none is a
//! literal in the code. The baseline policy is a second file, not a code path of its own.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReferenceCfg {
    /// e.g. "IR-"
    pub prefix: String,
    /// Characters after the prefix.
    pub length: usize,
    /// Repair a reference that is at most this many edits from a live one. 0 disables repair.
    pub max_repair_distance: usize,
    /// Only repair when the sender is the customer (or a declared third party is allowed).
    pub require_sender_match_for_repair: bool,
    /// Accept the reference only when it matches exactly as written (case and hyphen included):
    /// the strict reading of pay-by-reference.
    pub exact_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NamesCfg {
    /// Sender counts as the customer at or above this similarity.
    pub sender_match_threshold: f64,
    /// Groups of spelling variants; the first entry is canonical.
    pub variants: Vec<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScreeningCfg {
    /// "fuzzy" (normalised similarity) or "exact" (normalised equality).
    pub mode: String,
    /// Length of the sender-name field per rail, where the rail cuts names (ACH: 16).
    pub name_field_limits: Vec<(crate::domain::Rail, usize)>,
    /// Hold for review at or above this similarity to a listed name.
    pub hold_threshold: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResendCfg {
    pub enabled: bool,
    /// A second payment with the same sender account, Autoramp and amount inside this window is held.
    pub window_hours: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChainCfg {
    pub name: String,
    pub confirmations: u64,
    /// Re-submit a mint that is not in the canonical chain after this many seconds.
    pub resubmit_after_secs: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenCfg {
    pub ccy: crate::money::Ccy,
    pub symbol: String,
    pub decimals: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EngineConfig {
    pub policy_name: String,
    pub reference: ReferenceCfg,
    pub names: NamesCfg,
    pub screening: ScreeningCfg,
    pub resend: ResendCfg,
    /// Minimum deposit per currency in minor units; below it the transaction is `RejectedMinAmount`.
    pub minimums: Vec<(crate::money::Ccy, i64)>,
    /// Hold deposits from someone other than the customer unless the Autoramp allows third parties.
    pub hold_third_party_senders: bool,
    pub usd_window_open_hour_et: i64,
    pub usd_window_close_hour_et: i64,
    pub chains: Vec<ChainCfg>,
    pub tokens: Vec<TokenCfg>,
    /// Index returned deposits by their rail IDs so a trace finds them.
    pub trace_returned_deposits: bool,
    /// Treat a statement credit that never came with a notification as a deposit.
    pub ingest_statement_only_credits: bool,
}

impl EngineConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let cfg: EngineConfig = serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!((0.0..=1.0).contains(&self.names.sender_match_threshold), "sender_match_threshold must be in [0,1]");
        anyhow::ensure!((0.0..=1.0).contains(&self.screening.hold_threshold), "hold_threshold must be in [0,1]");
        anyhow::ensure!(matches!(self.screening.mode.as_str(), "fuzzy" | "exact"), "screening.mode must be fuzzy or exact");
        anyhow::ensure!(self.reference.length > 0, "reference.length must be positive");
        for t in &self.tokens {
            anyhow::ensure!(t.decimals >= t.ccy.scale(), "token {} has fewer decimals than its currency", t.symbol);
        }
        anyhow::ensure!(!self.chains.is_empty(), "at least one chain");
        Ok(())
    }

    pub fn minimum(&self, ccy: crate::money::Ccy) -> i64 {
        self.minimums.iter().find(|(c, _)| *c == ccy).map(|(_, v)| *v).unwrap_or(0)
    }

    pub fn token(&self, ccy: crate::money::Ccy) -> &TokenCfg {
        self.tokens.iter().find(|t| t.ccy == ccy).expect("a token per currency")
    }

    pub fn chain(&self, name: &str) -> Option<&ChainCfg> {
        self.chains.iter().find(|c| c.name == name)
    }
}

/// The repository root, found from the crate's manifest at build time or the working directory.
pub fn repo_root() -> std::path::PathBuf {
    let here = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if here.join("config").exists() { here } else { std::env::current_dir().expect("cwd") }
}

pub fn load_named(name: &str) -> anyhow::Result<EngineConfig> {
    EngineConfig::load(&repo_root().join("config").join(format!("{name}.json")))
}
