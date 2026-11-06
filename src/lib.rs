//! stablecoin-payin-engine: fiat pay-in to stablecoin mint.
//!
//! Bank files in (camt.054, pacs.008, NACHA, Fedwire, pacs.004, camt.053), each credit routed to its
//! Autoramp or returned, screened, held for people where needed, minted only against a funded reserve,
//! reconciled nightly against the bank and the chain, and traceable by any identifier a sender quotes.

pub mod api;
pub mod chain;
pub mod config;
pub mod domain;
pub mod engine;
pub mod eval;
pub mod ledger;
pub mod lifecycle;
pub mod money;
pub mod names;
pub mod rails;
pub mod report;
pub mod routing;
pub mod screening;
pub mod sim;
pub mod store;
pub mod time;
