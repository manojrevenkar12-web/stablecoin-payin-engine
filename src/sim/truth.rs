//! What really happened in a simulated world. Written by the simulation, read by the evaluation and by
//! the simulated operations desk (people who can look things up). The engine never sees it.

use crate::domain::Rail;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Kind {
    /// Paid to a dedicated virtual account.
    Clean,
    /// Pay-by-reference with the reference exactly as issued.
    ReferenceExact,
    /// Reference in another case, without the hyphen, with spaces, or split across lines.
    ReferenceReformatted,
    /// One-character typo by the customer itself.
    ReferenceTypoByCustomer,
    /// One-character typo by someone who is not the customer: not safe to repair.
    ReferenceTypoByStranger,
    /// Typo one edit away from two live references.
    ReferenceTypoAmbiguous,
    ReferenceMissing,
    CancelledAutoramp,
    /// Paid to an Autoramp of a suspended customer.
    SuspendedCustomer,
    UnknownAccount,
    /// The customer paid the same amount again by mistake (the first was still in review or slow).
    CustomerResend,
    /// A genuine second payment of the same amount (two invoices).
    LegitRepeat,
    /// Paid by someone other than the customer, on an Autoramp that does not accept third parties.
    ThirdPartyNotAllowed,
    /// Paid by a declared third party on an Autoramp that accepts them.
    ThirdPartyAllowed,
    SanctionedExact,
    SanctionedVariant,
    /// An innocent sender whose name resembles a listed one.
    NearMissInnocent,
    BelowMinimum,
    /// Customer under enhanced due diligence.
    EddCustomer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Expected {
    /// Should end up minted to `autoramp`.
    Credit,
    /// Should be sent back to the sender.
    Return,
    /// Should be held for a person, then follow `final_decision`.
    Hold,
    /// Should be rejected as below the minimum.
    RejectMin,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DepositTruth {
    pub rail: Rail,
    pub bank_ref: String,
    pub kind: Kind,
    pub amount_minor: i64,
    pub ccy: crate::money::Ccy,
    /// The Autoramp the sender meant to pay, if any.
    pub intended_autoramp: Option<String>,
    pub expected: Expected,
    /// Hold reason codes a correct engine raises (`sanctions_hit`, `possible_resend`, …).
    pub expected_holds: Vec<String>,
    /// What a person decides on review: "release", "reject" or "return".
    pub review_decision: Option<String>,
    /// For resends: the bank_ref of the first payment.
    pub resend_of: Option<String>,
    /// Not notified, only on the statement.
    pub statement_only: bool,
    /// Every ID the sender could quote when asking where the money is.
    pub trace_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WorldTruth {
    pub deposits: Vec<DepositTruth>,
    pub renotified: u64,
    pub corrupt_files: Vec<String>,
    pub fees: u64,
    pub fee_total_minor: i64,
    pub statement_only: u64,
    pub reorg_dropped_mints: u64,
    pub reorg_reincluded_mints: u64,
    pub payout_returns: u64,
    pub unregistered_wallet_redemptions: u64,
    pub redemptions: u64,
}
