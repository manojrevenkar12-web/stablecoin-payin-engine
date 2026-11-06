//! Double-entry ledger in integer minor units. Debits are positive, credits negative; every journal
//! sums to zero per currency, and after every journal the segregated reserve covers every token
//! that exists or is on its way to existing. A journal that would break either rule is refused whole.

use crate::money::Ccy;
use crate::time::Ts;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Account names. Kept as functions so the spelling lives in one place (and in the database schema).
pub mod acct {
    use crate::money::Ccy;
    pub fn operating(c: Ccy) -> String {
        format!("bank:{c}:operating")
    }
    pub fn reserve(c: Ccy) -> String {
        format!("bank:{c}:reserve")
    }
    /// Money received for a customer and not yet converted (held for review, waiting for the window).
    pub fn held(customer: &str, c: Ccy) -> String {
        format!("customer:{customer}:{c}:held")
    }
    /// Payouts owed to a customer after a burn, until the bank pays them.
    pub fn payout_due(customer: &str, c: Ccy) -> String {
        format!("customer:{customer}:{c}:payout_due")
    }
    pub fn tokens_pending(c: Ccy) -> String {
        format!("tokens:{c}:pending_mint")
    }
    pub fn tokens_outstanding(c: Ccy) -> String {
        format!("tokens:{c}:outstanding")
    }
    /// Unmatched deposits owed back to their senders.
    pub fn returns_due(c: Ccy) -> String {
        format!("returns:{c}:due")
    }
    /// Funds rejected on AML grounds, blocked pending a report.
    pub fn blocked(c: Ccy) -> String {
        format!("compliance:{c}:blocked")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Posting {
    pub account: String,
    pub ccy: Ccy,
    pub amount_minor: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub id: u64,
    pub at: Ts,
    pub kind: String,
    /// The deposit, payout or event this journal belongs to.
    pub reference: String,
    pub postings: Vec<Posting>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LedgerError {
    Unbalanced { ccy: Ccy, sum: i64 },
    ReserveShortfall { ccy: Ccy, reserve: i64, tokens: i64 },
    Empty,
    ZeroPosting,
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LedgerError::Unbalanced { ccy, sum } => write!(f, "journal does not balance in {ccy}: off by {sum}"),
            LedgerError::ReserveShortfall { ccy, reserve, tokens } => write!(f, "{ccy} reserve {reserve} would not cover {tokens} of tokens"),
            LedgerError::Empty => write!(f, "journal without postings"),
            LedgerError::ZeroPosting => write!(f, "zero posting"),
        }
    }
}
impl std::error::Error for LedgerError {}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Ledger {
    pub journals: Vec<Journal>,
    balances: BTreeMap<(String, Ccy), i64>,
    next_id: u64,
    /// Lowest reserve coverage seen after any journal, per currency (reserve − tokens, minor units).
    pub min_headroom: BTreeMap<Ccy, i64>,
}

impl Ledger {
    pub fn balance(&self, account: &str, ccy: Ccy) -> i64 {
        self.balances.get(&(account.to_string(), ccy)).copied().unwrap_or(0)
    }

    pub fn balances(&self) -> impl Iterator<Item = (&(String, Ccy), &i64)> {
        self.balances.iter()
    }

    /// Tokens that exist or are being minted, as a positive number.
    pub fn tokens(&self, ccy: Ccy) -> i64 {
        -(self.balance(&acct::tokens_outstanding(ccy), ccy) + self.balance(&acct::tokens_pending(ccy), ccy))
    }

    pub fn reserve(&self, ccy: Ccy) -> i64 {
        self.balance(&acct::reserve(ccy), ccy)
    }

    pub fn post(&mut self, at: Ts, kind: &str, reference: &str, postings: Vec<Posting>) -> Result<u64, LedgerError> {
        if postings.is_empty() {
            return Err(LedgerError::Empty);
        }
        if postings.iter().any(|p| p.amount_minor == 0) {
            return Err(LedgerError::ZeroPosting);
        }
        let mut sums: BTreeMap<Ccy, i64> = BTreeMap::new();
        for p in &postings {
            *sums.entry(p.ccy).or_default() += p.amount_minor;
        }
        if let Some((ccy, sum)) = sums.iter().find(|(_, s)| **s != 0) {
            return Err(LedgerError::Unbalanced { ccy: *ccy, sum: *sum });
        }
        // Check the reserve rule on the balances as they would be after this journal.
        for ccy in sums.keys() {
            let delta = |a: &str| postings.iter().filter(|p| p.ccy == *ccy && p.account == a).map(|p| p.amount_minor).sum::<i64>();
            let reserve = self.reserve(*ccy) + delta(&acct::reserve(*ccy));
            let tokens = -(self.balance(&acct::tokens_outstanding(*ccy), *ccy)
                + delta(&acct::tokens_outstanding(*ccy))
                + self.balance(&acct::tokens_pending(*ccy), *ccy)
                + delta(&acct::tokens_pending(*ccy)));
            if reserve < tokens {
                return Err(LedgerError::ReserveShortfall { ccy: *ccy, reserve, tokens });
            }
        }
        self.next_id += 1;
        let id = self.next_id;
        for p in &postings {
            *self.balances.entry((p.account.clone(), p.ccy)).or_default() += p.amount_minor;
        }
        for ccy in sums.keys() {
            let headroom = self.reserve(*ccy) - self.tokens(*ccy);
            let e = self.min_headroom.entry(*ccy).or_insert(headroom);
            *e = (*e).min(headroom);
        }
        self.journals.push(Journal { id, at, kind: kind.to_string(), reference: reference.to_string(), postings });
        Ok(id)
    }

    /// Sum of all balances per currency: always zero if every journal balanced.
    pub fn trial_balance(&self) -> BTreeMap<Ccy, i64> {
        let mut t = BTreeMap::new();
        for ((_, c), v) in &self.balances {
            *t.entry(*c).or_default() += v;
        }
        t
    }
}

pub fn dr(account: String, ccy: Ccy, v: i64) -> Posting {
    Posting { account, ccy, amount_minor: v }
}
pub fn cr(account: String, ccy: Ccy, v: i64) -> Posting {
    Posting { account, ccy, amount_minor: -v }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_unbalanced_and_empty_journals() {
        let mut l = Ledger::default();
        let e = l.post(0, "x", "r", vec![dr(acct::operating(Ccy::EUR), Ccy::EUR, 100), cr(acct::held("c", Ccy::EUR), Ccy::EUR, 99)]);
        assert_eq!(e, Err(LedgerError::Unbalanced { ccy: Ccy::EUR, sum: 1 }));
        assert_eq!(l.post(0, "x", "r", vec![]), Err(LedgerError::Empty));
        assert!(l.journals.is_empty());
    }

    #[test]
    fn cannot_mint_beyond_the_reserve() {
        let mut l = Ledger::default();
        let c = Ccy::USD;
        l.post(0, "deposit", "d1", vec![dr(acct::operating(c), c, 1000), cr(acct::held("c1", c), c, 1000)]).unwrap();
        // Minting without moving cash into the reserve is refused.
        let e = l.post(1, "mint", "d1", vec![dr(acct::held("c1", c), c, 1000), cr(acct::tokens_pending(c), c, 1000)]);
        assert!(matches!(e, Err(LedgerError::ReserveShortfall { .. })));
        // Minting and funding the reserve in one journal is accepted.
        l.post(
            1,
            "mint",
            "d1",
            vec![dr(acct::held("c1", c), c, 1000), cr(acct::tokens_pending(c), c, 1000), dr(acct::reserve(c), c, 1000), cr(acct::operating(c), c, 1000)],
        )
        .unwrap();
        assert_eq!(l.tokens(c), 1000);
        assert_eq!(l.reserve(c), 1000);
        assert_eq!(l.trial_balance()[&c], 0);
        assert_eq!(l.min_headroom[&c], 0);
    }
}
