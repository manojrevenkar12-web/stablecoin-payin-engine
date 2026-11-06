//! The engine's view of the platform: customers, Autoramps, inbound payments and their trace IDs.
//! Names follow MoonPay Enterprise's public API where it publishes one (Autoramp, `Approved`,
//! `pay_by_reference`, `IR-XXXXXX`, `FundsReviewInProgress`, …).

use crate::money::Ccy;
use crate::time::Ts;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Rail {
    /// SEPA credit transfer, notified with camt.054
    Sepa,
    /// SWIFT MX credit transfer, notified with pacs.008
    Swift,
    /// US ACH credit, NACHA file
    Ach,
    /// US Fedwire funds transfer
    Fedwire,
}

impl Rail {
    pub fn name(self) -> &'static str {
        match self {
            Rail::Sepa => "SEPA",
            Rail::Swift => "SWIFT",
            Rail::Ach => "ACH",
            Rail::Fedwire => "Fedwire",
        }
    }
    /// USD rails are subject to the mint-and-burn window.
    pub fn usd_window_applies(self, ccy: Ccy) -> bool {
        ccy == Ccy::USD
    }
}

/// Every identifier a bank or a sender might quote when asking "where is my money?".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceIds {
    pub uetr: Option<String>,
    pub end_to_end_id: Option<String>,
    pub acct_svcr_ref: Option<String>,
    pub ach_trace: Option<String>,
    pub imad: Option<String>,
}

impl TraceIds {
    pub fn all(&self) -> Vec<(&'static str, &str)> {
        let mut v = Vec::new();
        if let Some(x) = &self.uetr {
            v.push(("uetr", x.as_str()));
        }
        if let Some(x) = &self.end_to_end_id {
            v.push(("end_to_end_id", x.as_str()));
        }
        if let Some(x) = &self.acct_svcr_ref {
            v.push(("acct_svcr_ref", x.as_str()));
        }
        if let Some(x) = &self.ach_trace {
            v.push(("ach_trace", x.as_str()));
        }
        if let Some(x) = &self.imad {
            v.push(("imad", x.as_str()));
        }
        v
    }
}

/// One credit to one of our bank accounts, normalised from whichever rail format carried it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundPayment {
    pub rail: Rail,
    /// The identifier that makes this booking unique at our bank. A bank that re-sends the same
    /// notification re-sends the same `bank_ref`.
    pub bank_ref: String,
    pub ids: TraceIds,
    pub amount_minor: i64,
    pub ccy: Ccy,
    pub booked_at: Ts,
    pub debtor_name: String,
    pub debtor_account: String,
    /// Our account that was credited: a virtual IBAN, a virtual account number, or a shared
    /// pay-by-reference account.
    pub creditor_account: String,
    pub structured_ref: Option<String>,
    pub unstructured: Option<String>,
}

/// A bank telling us a payout we sent came back (pacs.004, ACH return).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayoutReturn {
    pub rail: Rail,
    pub bank_ref: String,
    pub original_ref: String,
    pub amount_minor: i64,
    pub ccy: Ccy,
    pub reason_code: String,
    pub booked_at: Ts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CustomerStatus {
    Active,
    /// Enhanced due diligence open: every deposit is held until it is decided.
    Edd,
    Suspended,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Customer {
    pub id: String,
    pub legal_name: String,
    #[serde(default)]
    pub trading_names: Vec<String>,
    pub status: CustomerStatus,
    /// Wallets registered (and Travel-Rule verified) for this customer: chain → addresses.
    pub wallets: Vec<(String, String)>,
    /// Account the customer redeems to: (rail, account number/IBAN, name).
    pub payout_account: (Rail, String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutorampStatus {
    Approved,
    Cancelled,
    EditPending,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Autoramp {
    pub id: String,
    pub customer_id: String,
    /// Fiat source currency; the destination is the issuance token for that currency.
    pub ccy: Ccy,
    pub chain: String,
    pub wallet: String,
    /// The account senders pay into. Dedicated for standing Autoramps, shared for pay-by-reference.
    pub deposit_account: String,
    pub pay_by_reference: bool,
    /// `IR-XXXXXX` when `pay_by_reference`.
    pub reference: Option<String>,
    /// `source_is_third_party`: accept deposits from senders other than the customer.
    pub third_party: bool,
    pub status: AutorampStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SanctionsEntry {
    pub id: String,
    pub names: Vec<String>,
    pub kind: String,
}

/// Everything the platform itself knows: customers, Autoramps, wallets, the sanctions list,
/// and its own bank accounts. Loaded from `platform.json`; never contains outcomes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Platform {
    pub customers: Vec<Customer>,
    pub autoramps: Vec<Autoramp>,
    pub sanctions: Vec<SanctionsEntry>,
    /// Our accounts at partner banks: (ccy, operating account id, reserve account id).
    pub bank_accounts: Vec<(Ccy, String, String)>,
    /// On-chain redemption address per chain.
    pub redemption_addresses: Vec<(String, String)>,
}

impl Platform {
    pub fn customer(&self, id: &str) -> Option<&Customer> {
        self.customers.iter().find(|c| c.id == id)
    }
    pub fn autoramp(&self, id: &str) -> Option<&Autoramp> {
        self.autoramps.iter().find(|a| a.id == id)
    }
    pub fn operating_account(&self, ccy: Ccy) -> Option<&str> {
        self.bank_accounts.iter().find(|(c, _, _)| *c == ccy).map(|(_, o, _)| o.as_str())
    }
    pub fn reserve_account(&self, ccy: Ccy) -> Option<&str> {
        self.bank_accounts.iter().find(|(c, _, _)| *c == ccy).map(|(_, _, r)| r.as_str())
    }
}
