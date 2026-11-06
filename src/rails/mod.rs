//! Bank-rail formats. `parse` recognises a file by its content, the way a bank-connectivity
//! service has to when files arrive on one SFTP inbox.

pub mod fedwire;
pub mod iso;
pub mod nacha;
pub mod xml;

use crate::domain::{InboundPayment, PayoutReturn};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    Camt054,
    Camt053,
    Pacs008,
    Pacs004,
    Nacha,
    Fedwire,
}

impl FileKind {
    pub fn name(self) -> &'static str {
        match self {
            FileKind::Camt054 => "camt.054",
            FileKind::Camt053 => "camt.053",
            FileKind::Pacs008 => "pacs.008",
            FileKind::Pacs004 => "pacs.004",
            FileKind::Nacha => "NACHA",
            FileKind::Fedwire => "Fedwire",
        }
    }
}

pub fn detect(raw: &str) -> Option<FileKind> {
    let t = raw.trim_start();
    if t.starts_with('<') {
        for (marker, kind) in [
            ("BkToCstmrDbtCdtNtfctn", FileKind::Camt054),
            ("BkToCstmrStmt", FileKind::Camt053),
            ("FIToFICstmrCdtTrf", FileKind::Pacs008),
            ("PmtRtr", FileKind::Pacs004),
        ] {
            if t.contains(marker) {
                return Some(kind);
            }
        }
        return None;
    }
    if t.starts_with("101") {
        return Some(FileKind::Nacha);
    }
    if t.starts_with("{1500}") || t.contains("{1520}") {
        return Some(FileKind::Fedwire);
    }
    None
}

#[derive(Clone, Debug)]
pub enum Parsed {
    Payments { credits: Vec<InboundPayment>, returns: Vec<PayoutReturn> },
    Statements(Vec<iso::Statement>),
}

pub fn parse(raw: &str) -> Result<(FileKind, Parsed)> {
    let Some(kind) = detect(raw) else { bail!("unrecognised file format") };
    let parsed = match kind {
        FileKind::Camt054 => Parsed::Payments { credits: iso::parse_camt054(raw)?, returns: vec![] },
        FileKind::Pacs008 => Parsed::Payments { credits: iso::parse_pacs008(raw)?, returns: vec![] },
        FileKind::Pacs004 => Parsed::Payments { credits: vec![], returns: iso::parse_pacs004(raw)? },
        FileKind::Nacha => {
            let (credits, returns) = nacha::parse(raw)?;
            Parsed::Payments { credits, returns }
        }
        FileKind::Fedwire => Parsed::Payments { credits: vec![fedwire::parse(raw)?], returns: vec![] },
        FileKind::Camt053 => Parsed::Statements(iso::parse_camt053(raw)?),
    };
    Ok((kind, parsed))
}
