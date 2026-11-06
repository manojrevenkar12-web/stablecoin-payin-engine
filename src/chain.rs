//! On-chain events and an indexer that only believes a block once it is buried deep enough, and
//! follows reorgs back to the common ancestor.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogKind {
    /// Supply created by our issuer contract. `op_id` is the mint request ID, which the contract
    /// refuses to use twice, so re-submitting a mint can never mint twice.
    Mint,
    /// Supply destroyed by our issuer contract; `op_id` is the burn ID.
    Burn,
    /// A token transfer between holders.
    Transfer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Log {
    pub tx_hash: String,
    pub kind: LogKind,
    pub token: String,
    pub op_id: Option<String>,
    pub from: String,
    pub to: String,
    pub amount_base: i128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub chain: String,
    pub number: u64,
    pub hash: String,
    pub parent: String,
    pub time: i64,
    pub logs: Vec<Log>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct IndexerStats {
    pub blocks_seen: u64,
    pub reorgs: u64,
    pub deepest_reorg: u64,
    /// Reorgs deeper than the confirmation depth: logs already acted on may be gone. Must stay 0.
    pub reorgs_past_confirmation: u64,
}

/// Follows one chain. `confirmed` returns the logs of blocks that just became final.
#[derive(Debug, Clone, Default)]
pub struct Indexer {
    pub chain: String,
    pub confirmations: u64,
    blocks: HashMap<String, Block>,
    /// Canonical hashes by height.
    canonical: Vec<String>,
    /// Highest height whose logs have been released.
    released: Option<u64>,
    pub stats: IndexerStats,
}

impl Indexer {
    pub fn new(chain: &str, confirmations: u64) -> Self {
        Indexer { chain: chain.to_string(), confirmations, ..Default::default() }
    }

    pub fn head(&self) -> Option<u64> {
        self.canonical.len().checked_sub(1).map(|h| h as u64)
    }

    /// Ops seen in canonical blocks that are not final yet (so not to be re-submitted).
    pub fn unconfirmed_ops(&self) -> Vec<String> {
        let from = self.released.map(|r| r as usize + 1).unwrap_or(0);
        self.canonical[from.min(self.canonical.len())..]
            .iter()
            .filter_map(|h| self.blocks.get(h))
            .flat_map(|b| b.logs.iter().filter_map(|l| l.op_id.clone()))
            .collect()
    }

    /// Add a block. Returns the logs of every block that became final as a result, in order.
    pub fn push(&mut self, b: Block) -> Result<Vec<Log>, String> {
        if b.chain != self.chain {
            return Err(format!("block for {} sent to the {} indexer", b.chain, self.chain));
        }
        self.stats.blocks_seen += 1;
        let n = b.number as usize;
        if n > self.canonical.len() {
            return Err(format!("{}: gap before block {}", self.chain, b.number));
        }
        if n > 0 {
            let parent_ok = self.canonical.get(n - 1) == Some(&b.parent);
            if !parent_ok {
                return Err(format!("{}: block {} does not build on our chain", self.chain, b.number));
            }
        }
        if n < self.canonical.len() {
            // A competing block at a height we already have: reorg back to its parent.
            let depth = (self.canonical.len() - n) as u64;
            self.stats.reorgs += 1;
            self.stats.deepest_reorg = self.stats.deepest_reorg.max(depth);
            if self.released.is_some_and(|r| r as usize >= n) {
                self.stats.reorgs_past_confirmation += 1;
                self.released = n.checked_sub(1).map(|x| x as u64);
            }
            self.canonical.truncate(n);
        }
        self.canonical.push(b.hash.clone());
        self.blocks.insert(b.hash.clone(), b);
        let head = self.canonical.len() as u64 - 1;
        let mut out = vec![];
        if head >= self.confirmations {
            let final_height = head - self.confirmations;
            let start = self.released.map(|r| r + 1).unwrap_or(0);
            for h in start..=final_height {
                let block = &self.blocks[&self.canonical[h as usize]];
                out.extend(block.logs.iter().cloned());
            }
            if start <= final_height {
                self.released = Some(final_height);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blk(n: u64, hash: &str, parent: &str, op: Option<&str>) -> Block {
        Block {
            chain: "ethereum".into(),
            number: n,
            hash: hash.into(),
            parent: parent.into(),
            time: n as i64 * 12,
            logs: op
                .map(|o| {
                    vec![Log {
                        tx_hash: format!("tx-{o}"),
                        kind: LogKind::Mint,
                        token: "XUSD".into(),
                        op_id: Some(o.into()),
                        from: "0x0".into(),
                        to: "0xa".into(),
                        amount_base: 1,
                    }]
                })
                .unwrap_or_default(),
        }
    }

    #[test]
    fn releases_only_after_confirmations() {
        let mut ix = Indexer::new("ethereum", 2);
        assert!(ix.push(blk(0, "g", "", None)).unwrap().is_empty());
        assert!(ix.push(blk(1, "a1", "g", Some("m1"))).unwrap().is_empty());
        assert!(ix.push(blk(2, "a2", "a1", None)).unwrap().is_empty());
        let out = ix.push(blk(3, "a3", "a2", None)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].op_id.as_deref(), Some("m1"));
    }

    #[test]
    fn a_shallow_reorg_drops_the_unconfirmed_mint() {
        let mut ix = Indexer::new("ethereum", 3);
        ix.push(blk(0, "g", "", None)).unwrap();
        ix.push(blk(1, "a1", "g", Some("m1"))).unwrap();
        ix.push(blk(2, "a2", "a1", None)).unwrap();
        assert_eq!(ix.unconfirmed_ops(), vec!["m1".to_string()]);
        // Competing fork from genesis without the mint.
        ix.push(blk(1, "b1", "g", None)).unwrap();
        assert!(ix.unconfirmed_ops().is_empty());
        let mut released = vec![];
        for (n, h, p) in [(2, "b2", "b1"), (3, "b3", "b2"), (4, "b4", "b3"), (5, "b5", "b4")] {
            released.extend(ix.push(blk(n, h, p, None)).unwrap());
        }
        assert!(released.is_empty(), "the dropped mint is never released");
        assert_eq!(ix.stats.reorgs, 1);
        assert_eq!(ix.stats.reorgs_past_confirmation, 0);
    }

    #[test]
    fn refuses_gaps_and_foreign_parents() {
        let mut ix = Indexer::new("ethereum", 1);
        ix.push(blk(0, "g", "", None)).unwrap();
        assert!(ix.push(blk(2, "x", "y", None)).is_err());
        assert!(ix.push(blk(1, "x", "nope", None)).is_err());
    }
}
