//! ISO 20022 messages the engine reads, and writers the simulated bank uses to produce them:
//! camt.054 (credit notification), camt.053 (end-of-day statement), pacs.008 (customer credit
//! transfer) and pacs.004 (payment return).

use super::xml::{self, Node, esc};
use crate::domain::{InboundPayment, PayoutReturn, Rail, TraceIds};
use crate::money::{Ccy, format_decimal, parse_decimal};
use crate::time::{Ts, fmt_date, fmt_iso, parse_iso};
use anyhow::{Context, Result, anyhow, bail};

/// The one identifier that names a booking at our bank, per rail. Notifications and statements
/// must agree on it, so reconciliation can match them.
pub fn bank_ref_for(rail: Rail, ids: &TraceIds) -> Option<String> {
    match rail {
        Rail::Sepa => ids.acct_svcr_ref.clone(),
        Rail::Swift => ids.uetr.clone(),
        Rail::Ach => ids.ach_trace.clone(),
        Rail::Fedwire => ids.imad.clone(),
    }
}

fn rail_from_subfamily(code: &str) -> Option<Rail> {
    match code {
        "ESCT" => Some(Rail::Sepa),
        "XBCT" => Some(Rail::Swift),
        "ACDT" => Some(Rail::Ach),
        "DMCT" => Some(Rail::Fedwire),
        _ => None,
    }
}

pub fn subfamily_for(rail: Rail) -> &'static str {
    match rail {
        Rail::Sepa => "ESCT",
        Rail::Swift => "XBCT",
        Rail::Ach => "ACDT",
        Rail::Fedwire => "DMCT",
    }
}

fn amount(n: &Node, path: &[&str]) -> Result<(i64, Ccy)> {
    let node = n.path(path).ok_or_else(|| anyhow!("missing {}", path.join("/")))?;
    let ccy = node.attr("Ccy").and_then(Ccy::parse).ok_or_else(|| anyhow!("unsupported or missing Ccy on {}", path.join("/")))?;
    let v = parse_decimal(&node.text, ccy).with_context(|| format!("amount at {}", path.join("/")))?;
    Ok((v, ccy))
}

fn account_id(n: Option<&Node>) -> Option<String> {
    let n = n?;
    n.text_any(&[&["Id", "IBAN"], &["Id", "Othr", "Id"]]).map(|s| s.replace(' ', ""))
}

fn party_name(n: &Node, party: &str) -> Option<String> {
    // camt.054.001.08 nests the party under Pty; older versions do not.
    n.text_any(&[&["RltdPties", party, "Pty", "Nm"], &["RltdPties", party, "Nm"], &[party, "Nm"]])
}

fn trace_ids_from_refs(refs: Option<&Node>) -> TraceIds {
    let mut ids = TraceIds::default();
    if let Some(r) = refs {
        ids.end_to_end_id = r.text_at(&["EndToEndId"]).filter(|s| s != "NOTPROVIDED");
        ids.uetr = r.text_at(&["UETR"]).map(|s| s.to_lowercase());
        ids.acct_svcr_ref = r.text_at(&["AcctSvcrRef"]);
        ids.imad = r.text_at(&["ClrSysRef"]);
        for p in r.children_named("Prtry") {
            if p.text_at(&["Tp"]).as_deref() == Some("ACH_TRACE") {
                ids.ach_trace = p.text_at(&["Ref"]);
            }
        }
    }
    ids
}

fn remittance(tx: &Node) -> (Option<String>, Option<String>) {
    let rmt = tx.child("RmtInf");
    let structured = rmt.and_then(|r| r.text_at(&["Strd", "CdtrRefInf", "Ref"]));
    let unstructured = rmt.map(|r| r.children_named("Ustrd").map(|u| u.text.trim().to_string()).collect::<Vec<_>>().join(" ")).filter(|s| !s.trim().is_empty());
    (structured, unstructured)
}

fn booking_time(ntry: &Node) -> Option<Ts> {
    ntry.text_any(&[&["BookgDt", "DtTm"], &["BookgDt", "Dt"], &["ValDt", "Dt"]]).and_then(|s| parse_iso(&s))
}

/// Credits from a statement or notification entry, one per transaction detail.
fn credits_from_entry(ntry: &Node, our_account: &str) -> Result<Vec<InboundPayment>> {
    if ntry.text_at(&["CdtDbtInd"]).as_deref() != Some("CRDT") {
        return Ok(vec![]);
    }
    let subfam = ntry.text_at(&["BkTxCd", "Domn", "Fmly", "SubFmlyCd"]).unwrap_or_default();
    let fam = ntry.text_at(&["BkTxCd", "Domn", "Fmly", "Cd"]).unwrap_or_default();
    if fam != "RCDT" {
        return Ok(vec![]); // returns, book transfers and fees are not customer deposits
    }
    let Some(rail) = rail_from_subfamily(&subfam) else { return Ok(vec![]) };
    let booked = booking_time(ntry).ok_or_else(|| anyhow!("entry without a booking date"))?;
    let entry_svcr = ntry.text_at(&["AcctSvcrRef"]);
    let mut out = vec![];
    let details: Vec<&Node> = ntry.children_named("NtryDtls").flat_map(|d| d.children_named("TxDtls")).collect();
    if details.is_empty() {
        bail!("credit entry {:?} has no transaction details", entry_svcr);
    }
    let single = details.len() == 1;
    for tx in details {
        let (amt, ccy) = if tx.path(&["Amt"]).is_some() {
            amount(tx, &["Amt"])?
        } else if single {
            amount(ntry, &["Amt"])?
        } else {
            bail!("batched entry without per-transaction amounts");
        };
        let mut ids = trace_ids_from_refs(tx.child("Refs"));
        if ids.acct_svcr_ref.is_none() && single {
            ids.acct_svcr_ref = entry_svcr.clone();
        }
        let bank_ref = bank_ref_for(rail, &ids).ok_or_else(|| anyhow!("{} credit without its bank reference", rail.name()))?;
        let (structured_ref, unstructured) = remittance(tx);
        out.push(InboundPayment {
            rail,
            bank_ref,
            ids,
            amount_minor: amt,
            ccy,
            booked_at: booked,
            debtor_name: party_name(tx, "Dbtr").unwrap_or_default(),
            debtor_account: account_id(tx.path(&["RltdPties", "DbtrAcct"])).unwrap_or_default(),
            creditor_account: account_id(tx.path(&["RltdPties", "CdtrAcct"])).unwrap_or_else(|| our_account.to_string()),
            structured_ref,
            unstructured,
        });
    }
    Ok(out)
}

/// camt.054: credits booked on our account.
pub fn parse_camt054(s: &str) -> Result<Vec<InboundPayment>> {
    let doc = xml::parse(s)?;
    let root = doc.child("BkToCstmrDbtCdtNtfctn").ok_or_else(|| anyhow!("not a camt.054"))?;
    let mut out = vec![];
    for ntfctn in root.children_named("Ntfctn") {
        let acct = account_id(ntfctn.child("Acct")).unwrap_or_default();
        for ntry in ntfctn.children_named("Ntry") {
            out.extend(credits_from_entry(ntry, &acct)?);
        }
    }
    Ok(out)
}

/// pacs.008: SWIFT MX customer credit transfers to our account.
pub fn parse_pacs008(s: &str) -> Result<Vec<InboundPayment>> {
    let doc = xml::parse(s)?;
    let root = doc.child("FIToFICstmrCdtTrf").ok_or_else(|| anyhow!("not a pacs.008"))?;
    let created = root.text_at(&["GrpHdr", "CreDtTm"]).and_then(|s| parse_iso(&s));
    let mut out = vec![];
    for tx in root.children_named("CdtTrfTxInf") {
        let (amt, ccy) = amount(tx, &["IntrBkSttlmAmt"])?;
        let uetr = tx.text_at(&["PmtId", "UETR"]).map(|s| s.to_lowercase()).ok_or_else(|| anyhow!("pacs.008 without UETR"))?;
        let booked = tx.text_at(&["IntrBkSttlmDt"]).and_then(|s| parse_iso(&s)).or(created).ok_or_else(|| anyhow!("no settlement date"))?;
        let ids =
            TraceIds { uetr: Some(uetr.clone()), end_to_end_id: tx.text_at(&["PmtId", "EndToEndId"]).filter(|s| s != "NOTPROVIDED"), ..Default::default() };
        let (structured_ref, unstructured) = remittance(tx);
        out.push(InboundPayment {
            rail: Rail::Swift,
            bank_ref: uetr,
            ids,
            amount_minor: amt,
            ccy,
            booked_at: created.unwrap_or(booked),
            debtor_name: tx.text_at(&["Dbtr", "Nm"]).unwrap_or_default(),
            debtor_account: account_id(tx.child("DbtrAcct")).unwrap_or_default(),
            creditor_account: account_id(tx.child("CdtrAcct")).ok_or_else(|| anyhow!("pacs.008 without creditor account"))?,
            structured_ref,
            unstructured,
        });
    }
    Ok(out)
}

/// pacs.004: payouts returned to us.
pub fn parse_pacs004(s: &str) -> Result<Vec<PayoutReturn>> {
    let doc = xml::parse(s)?;
    let root = doc.child("PmtRtr").ok_or_else(|| anyhow!("not a pacs.004"))?;
    let created = root.text_at(&["GrpHdr", "CreDtTm"]).and_then(|s| parse_iso(&s)).ok_or_else(|| anyhow!("no creation time"))?;
    let mut out = vec![];
    for tx in root.children_named("TxInf") {
        let (amt, ccy) = amount(tx, &["RtrdIntrBkSttlmAmt"])?;
        let original =
            tx.text_any(&[&["OrgnlUETR"], &["OrgnlEndToEndId"]]).map(|s| s.to_lowercase()).ok_or_else(|| anyhow!("pacs.004 without original reference"))?;
        out.push(PayoutReturn {
            rail: Rail::Sepa,
            bank_ref: tx.text_at(&["RtrId"]).ok_or_else(|| anyhow!("pacs.004 without RtrId"))?,
            original_ref: original,
            amount_minor: amt,
            ccy,
            reason_code: tx.text_at(&["RtrRsnInf", "Rsn", "Cd"]).unwrap_or_default(),
            booked_at: created,
        });
    }
    Ok(out)
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EntryKind {
    /// A customer credit (RCDT): a deposit.
    Credit,
    /// A payment we sent out (ICDT): a payout, or a deposit we returned to its sender.
    Outgoing,
    /// One of our payouts coming back (RCDT/RRTN).
    ReturnReceived,
    /// Bank charges (ACMT/MDOP/CHRG).
    Fee,
    /// Book transfer between our own accounts (BOOK), e.g. a reserve sweep.
    Transfer,
    Other(String),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StatementEntry {
    pub bank_ref: String,
    /// Signed: credits positive, debits negative.
    pub amount_minor: i64,
    pub kind: EntryKind,
    pub booked_at: Ts,
    /// Our own reference on transfers we initiated (EndToEndId we set).
    pub our_ref: Option<String>,
    pub credit: Option<InboundPayment>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Statement {
    pub account: String,
    pub ccy: Ccy,
    pub date: Ts,
    pub opening_minor: i64,
    pub closing_minor: i64,
    pub entries: Vec<StatementEntry>,
}

fn signed_balance(stmt: &Node, code: &str) -> Result<Option<i64>> {
    for bal in stmt.children_named("Bal") {
        if bal.text_at(&["Tp", "CdOrPrtry", "Cd"]).as_deref() == Some(code) {
            let (v, _) = amount(bal, &["Amt"])?;
            let sign = if bal.text_at(&["CdtDbtInd"]).as_deref() == Some("DBIT") { -1 } else { 1 };
            return Ok(Some(sign * v));
        }
    }
    Ok(None)
}

/// camt.053: the bank's end-of-day statement for one account.
pub fn parse_camt053(s: &str) -> Result<Vec<Statement>> {
    let doc = xml::parse(s)?;
    let root = doc.child("BkToCstmrStmt").ok_or_else(|| anyhow!("not a camt.053"))?;
    let mut out = vec![];
    for stmt in root.children_named("Stmt") {
        let account = account_id(stmt.child("Acct")).ok_or_else(|| anyhow!("statement without account"))?;
        let ccy = stmt.text_at(&["Acct", "Ccy"]).and_then(|c| Ccy::parse(&c)).ok_or_else(|| anyhow!("statement without currency"))?;
        let opening = signed_balance(stmt, "OPBD")?.ok_or_else(|| anyhow!("no opening balance"))?;
        let closing = signed_balance(stmt, "CLBD")?.ok_or_else(|| anyhow!("no closing balance"))?;
        let date = stmt.text_any(&[&["FrToDt", "ToDtTm"], &["CreDtTm"]]).and_then(|s| parse_iso(&s)).ok_or_else(|| anyhow!("no statement date"))?;
        let mut entries = vec![];
        for ntry in stmt.children_named("Ntry") {
            let (amt, _) = amount(ntry, &["Amt"])?;
            let credit_ind = ntry.text_at(&["CdtDbtInd"]).unwrap_or_default();
            let signed = if credit_ind == "DBIT" { -amt } else { amt };
            let dom = ntry.text_at(&["BkTxCd", "Domn", "Cd"]).unwrap_or_default();
            let fam = ntry.text_at(&["BkTxCd", "Domn", "Fmly", "Cd"]).unwrap_or_default();
            let sub = ntry.text_at(&["BkTxCd", "Domn", "Fmly", "SubFmlyCd"]).unwrap_or_default();
            let kind = match (dom.as_str(), fam.as_str(), sub.as_str()) {
                ("ACMT", _, "CHRG") => EntryKind::Fee,
                ("PMNT", _, "BOOK") => EntryKind::Transfer,
                ("PMNT", "RCDT", s) if rail_from_subfamily(s).is_some() => EntryKind::Credit,
                ("PMNT", "RCDT", "RRTN") => EntryKind::ReturnReceived,
                ("PMNT", "ICDT", _) => EntryKind::Outgoing,
                _ => EntryKind::Other(format!("{dom}/{fam}/{sub}")),
            };
            let booked = booking_time(ntry).ok_or_else(|| anyhow!("entry without booking date"))?;
            let credit = if kind == EntryKind::Credit { credits_from_entry(ntry, &account)?.into_iter().next() } else { None };
            let svcr = ntry.text_at(&["AcctSvcrRef"]).unwrap_or_default();
            let bank_ref = credit.as_ref().map(|c| c.bank_ref.clone()).unwrap_or(svcr);
            let our_ref = ntry.text_at(&["NtryDtls", "TxDtls", "Refs", "EndToEndId"]);
            entries.push(StatementEntry { bank_ref, amount_minor: signed, kind, booked_at: booked, our_ref, credit });
        }
        out.push(Statement { account, ccy, date, opening_minor: opening, closing_minor: closing, entries });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Writers (used by the simulated bank and by tests)

fn acct_xml(tag: &str, id: &str) -> String {
    if id.len() >= 15 && id.chars().take(2).all(|c| c.is_ascii_uppercase()) {
        format!("<{tag}><Id><IBAN>{}</IBAN></Id></{tag}>", esc(id))
    } else {
        format!("<{tag}><Id><Othr><Id>{}</Id></Othr></Id></{tag}>", esc(id))
    }
}

fn refs_xml(ids: &TraceIds) -> String {
    let mut s = String::from("<Refs>");
    if let Some(v) = &ids.acct_svcr_ref {
        s += &format!("<AcctSvcrRef>{}</AcctSvcrRef>", esc(v));
    }
    s += &format!("<EndToEndId>{}</EndToEndId>", esc(ids.end_to_end_id.as_deref().unwrap_or("NOTPROVIDED")));
    if let Some(v) = &ids.uetr {
        s += &format!("<UETR>{}</UETR>", esc(v));
    }
    if let Some(v) = &ids.imad {
        s += &format!("<ClrSysRef>{}</ClrSysRef>", esc(v));
    }
    if let Some(v) = &ids.ach_trace {
        s += &format!("<Prtry><Tp>ACH_TRACE</Tp><Ref>{}</Ref></Prtry>", esc(v));
    }
    s + "</Refs>"
}

fn rmt_xml(structured: &Option<String>, unstructured: &Option<String>) -> String {
    let mut s = String::new();
    if structured.is_none() && unstructured.is_none() {
        return s;
    }
    s += "<RmtInf>";
    if let Some(u) = unstructured {
        s += &format!("<Ustrd>{}</Ustrd>", esc(u));
    }
    if let Some(r) = structured {
        s += &format!("<Strd><CdtrRefInf><Tp><CdOrPrtry><Cd>SCOR</Cd></CdOrPrtry></Tp><Ref>{}</Ref></CdtrRefInf></Strd>", esc(r));
    }
    s + "</RmtInf>"
}

/// The `<Ntry>` for one credit, used by both camt.054 and camt.053.
pub fn credit_entry_xml(p: &InboundPayment, entry_ref: &str) -> String {
    format!(
        "<Ntry><Amt Ccy=\"{ccy}\">{amt}</Amt><CdtDbtInd>CRDT</CdtDbtInd><Sts><Cd>BOOK</Cd></Sts>\
<BookgDt><DtTm>{bk}</DtTm></BookgDt><ValDt><Dt>{vd}</Dt></ValDt><AcctSvcrRef>{er}</AcctSvcrRef>\
<BkTxCd><Domn><Cd>PMNT</Cd><Fmly><Cd>RCDT</Cd><SubFmlyCd>{sub}</SubFmlyCd></Fmly></Domn></BkTxCd>\
<NtryDtls><TxDtls>{refs}<Amt Ccy=\"{ccy}\">{amt}</Amt><RltdPties><Dbtr><Pty><Nm>{dn}</Nm></Pty></Dbtr>{da}{ca}</RltdPties>{rmt}</TxDtls></NtryDtls></Ntry>",
        ccy = p.ccy,
        amt = format_decimal(p.amount_minor, p.ccy),
        bk = fmt_iso(p.booked_at),
        vd = fmt_date(p.booked_at),
        er = esc(entry_ref),
        sub = subfamily_for(p.rail),
        refs = refs_xml(&p.ids),
        dn = esc(&p.debtor_name),
        da = acct_xml("DbtrAcct", &p.debtor_account),
        ca = acct_xml("CdtrAcct", &p.creditor_account),
        rmt = rmt_xml(&p.structured_ref, &p.unstructured),
    )
}

pub fn write_camt054(msg_id: &str, created: Ts, account: &str, ccy: Ccy, credits: &[InboundPayment]) -> String {
    let entries: String = credits.iter().map(|p| credit_entry_xml(p, p.ids.acct_svcr_ref.as_deref().unwrap_or(&p.bank_ref))).collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Document xmlns=\"urn:iso:std:iso:20022:tech:xsd:camt.054.001.08\"><BkToCstmrDbtCdtNtfctn>\
<GrpHdr><MsgId>{m}</MsgId><CreDtTm>{c}</CreDtTm></GrpHdr><Ntfctn><Id>{m}-1</Id><CreDtTm>{c}</CreDtTm>\
<Acct>{acct}<Ccy>{ccy}</Ccy></Acct>{entries}</Ntfctn></BkToCstmrDbtCdtNtfctn></Document>\n",
        m = esc(msg_id),
        c = fmt_iso(created),
        acct = acct_xml("X", account).trim_start_matches("<X>").trim_end_matches("</X>"),
    )
}

pub fn write_pacs008(msg_id: &str, created: Ts, p: &InboundPayment, debtor_bic: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Document xmlns=\"urn:iso:std:iso:20022:tech:xsd:pacs.008.001.08\"><FIToFICstmrCdtTrf>\
<GrpHdr><MsgId>{m}</MsgId><CreDtTm>{c}</CreDtTm><NbOfTxs>1</NbOfTxs><SttlmInf><SttlmMtd>INDA</SttlmMtd></SttlmInf></GrpHdr>\
<CdtTrfTxInf><PmtId><InstrId>{m}</InstrId><EndToEndId>{e2e}</EndToEndId><UETR>{uetr}</UETR></PmtId>\
<IntrBkSttlmAmt Ccy=\"{ccy}\">{amt}</IntrBkSttlmAmt><IntrBkSttlmDt>{d}</IntrBkSttlmDt><ChrgBr>SHAR</ChrgBr>\
<Dbtr><Nm>{dn}</Nm></Dbtr>{da}<DbtrAgt><FinInstnId><BICFI>{bic}</BICFI></FinInstnId></DbtrAgt>\
<CdtrAgt><FinInstnId><BICFI>PIEBDEBBXXX</BICFI></FinInstnId></CdtrAgt><Cdtr><Nm>MoonPay Enterprise client account</Nm></Cdtr>{ca}{rmt}</CdtTrfTxInf></FIToFICstmrCdtTrf></Document>\n",
        m = esc(msg_id),
        c = fmt_iso(created),
        e2e = esc(p.ids.end_to_end_id.as_deref().unwrap_or("NOTPROVIDED")),
        uetr = esc(p.ids.uetr.as_deref().unwrap_or("")),
        ccy = p.ccy,
        amt = format_decimal(p.amount_minor, p.ccy),
        d = fmt_date(p.booked_at),
        dn = esc(&p.debtor_name),
        da = acct_xml("DbtrAcct", &p.debtor_account),
        bic = esc(debtor_bic),
        ca = acct_xml("CdtrAcct", &p.creditor_account),
        rmt = rmt_xml(&p.structured_ref, &p.unstructured),
    )
}

pub fn write_pacs004(msg_id: &str, created: Ts, r: &PayoutReturn) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Document xmlns=\"urn:iso:std:iso:20022:tech:xsd:pacs.004.001.09\"><PmtRtr>\
<GrpHdr><MsgId>{m}</MsgId><CreDtTm>{c}</CreDtTm><NbOfTxs>1</NbOfTxs><SttlmInf><SttlmMtd>INDA</SttlmMtd></SttlmInf></GrpHdr>\
<TxInf><RtrId>{rid}</RtrId><OrgnlEndToEndId>{orig}</OrgnlEndToEndId><RtrdIntrBkSttlmAmt Ccy=\"{ccy}\">{amt}</RtrdIntrBkSttlmAmt>\
<IntrBkSttlmDt>{d}</IntrBkSttlmDt><RtrRsnInf><Rsn><Cd>{rc}</Cd></Rsn></RtrRsnInf></TxInf></PmtRtr></Document>\n",
        m = esc(msg_id),
        c = fmt_iso(created),
        rid = esc(&r.bank_ref),
        orig = esc(&r.original_ref),
        ccy = r.ccy,
        amt = format_decimal(r.amount_minor, r.ccy),
        d = fmt_date(r.booked_at),
        rc = esc(&r.reason_code),
    )
}

/// One non-customer statement line (fee, transfer, outgoing payment, return we sent).
pub fn other_entry_xml(e: &StatementEntry, ccy: Ccy) -> String {
    let (dom, fam, sub) = match &e.kind {
        EntryKind::Fee => ("ACMT", "MDOP", "CHRG"),
        EntryKind::Transfer => ("PMNT", if e.amount_minor >= 0 { "RCDT" } else { "ICDT" }, "BOOK"),
        EntryKind::Outgoing => ("PMNT", "ICDT", "ESCT"),
        EntryKind::ReturnReceived | EntryKind::Credit => ("PMNT", "RCDT", "RRTN"),
        EntryKind::Other(_) => ("XTND", "NTAV", "NTAV"),
    };
    let ind = if e.amount_minor < 0 { "DBIT" } else { "CRDT" };
    let refs =
        e.our_ref.as_ref().map(|r| format!("<NtryDtls><TxDtls><Refs><EndToEndId>{}</EndToEndId></Refs></TxDtls></NtryDtls>", esc(r))).unwrap_or_default();
    format!(
        "<Ntry><Amt Ccy=\"{ccy}\">{amt}</Amt><CdtDbtInd>{ind}</CdtDbtInd><Sts><Cd>BOOK</Cd></Sts><BookgDt><DtTm>{bk}</DtTm></BookgDt>\
<AcctSvcrRef>{r}</AcctSvcrRef><BkTxCd><Domn><Cd>{dom}</Cd><Fmly><Cd>{fam}</Cd><SubFmlyCd>{sub}</SubFmlyCd></Fmly></Domn></BkTxCd>{refs}</Ntry>",
        amt = format_decimal(e.amount_minor.abs(), ccy),
        bk = fmt_iso(e.booked_at),
        r = esc(&e.bank_ref),
    )
}

pub fn write_camt053(msg_id: &str, s: &Statement) -> String {
    let bal = |code: &str, v: i64| {
        format!(
            "<Bal><Tp><CdOrPrtry><Cd>{code}</Cd></CdOrPrtry></Tp><Amt Ccy=\"{ccy}\">{amt}</Amt><CdtDbtInd>{ind}</CdtDbtInd><Dt><Dt>{d}</Dt></Dt></Bal>",
            ccy = s.ccy,
            amt = format_decimal(v.abs(), s.ccy),
            ind = if v < 0 { "DBIT" } else { "CRDT" },
            d = fmt_date(s.date),
        )
    };
    let entries: String = s
        .entries
        .iter()
        .map(|e| match (&e.kind, &e.credit) {
            (EntryKind::Credit, Some(p)) => credit_entry_xml(p, p.ids.acct_svcr_ref.as_deref().unwrap_or(&p.bank_ref)),
            _ => other_entry_xml(e, s.ccy),
        })
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Document xmlns=\"urn:iso:std:iso:20022:tech:xsd:camt.053.001.08\"><BkToCstmrStmt>\
<GrpHdr><MsgId>{m}</MsgId><CreDtTm>{c}</CreDtTm></GrpHdr><Stmt><Id>{m}-1</Id><CreDtTm>{c}</CreDtTm><FrToDt><FrDtTm>{d}T00:00:00Z</FrDtTm><ToDtTm>{c}</ToDtTm></FrToDt>\
<Acct>{acct}<Ccy>{ccy}</Ccy></Acct>{ob}{cb}{entries}</Stmt></BkToCstmrStmt></Document>\n",
        m = esc(msg_id),
        c = fmt_iso(s.date),
        d = fmt_date(s.date),
        acct = acct_xml("X", &s.account).trim_start_matches("<X>").trim_end_matches("</X>"),
        ccy = s.ccy,
        ob = bal("OPBD", s.opening_minor),
        cb = bal("CLBD", s.closing_minor),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::ts;

    fn sepa_credit() -> InboundPayment {
        InboundPayment {
            rail: Rail::Sepa,
            bank_ref: "2026110200001234".into(),
            ids: TraceIds { acct_svcr_ref: Some("2026110200001234".into()), end_to_end_id: Some("INV-2026-118".into()), ..Default::default() },
            amount_minor: 1_250_000,
            ccy: Ccy::EUR,
            booked_at: ts(2026, 11, 2, 9, 14, 0),
            debtor_name: "Müller & Söhne GmbH".into(),
            debtor_account: "DE89370400440532013000".into(),
            creditor_account: "DE44500105175407324931".into(),
            structured_ref: None,
            unstructured: Some("Payment IR-7KQ2MX invoice 118".into()),
        }
    }

    #[test]
    fn camt054_round_trip() {
        let p = sepa_credit();
        let x = write_camt054("N1", p.booked_at, "DE44500105175407324931", Ccy::EUR, std::slice::from_ref(&p));
        assert_eq!(parse_camt054(&x).unwrap(), vec![p]);
    }

    #[test]
    fn camt054_from_a_bank_with_older_layout() {
        // Hand-written in the camt.054.001.02 style: party name without Pty, entry-level amount only,
        // the reference in a structured creditor reference.
        let x = r#"<Document xmlns="urn:iso:std:iso:20022:tech:xsd:camt.054.001.02"><BkToCstmrDbtCdtNtfctn><GrpHdr><MsgId>X</MsgId></GrpHdr>
<Ntfctn><Acct><Id><IBAN>DE44 5001 0517 5407 3249 31</IBAN></Id></Acct>
<Ntry><Amt Ccy="EUR">250.00</Amt><CdtDbtInd>CRDT</CdtDbtInd><BookgDt><Dt>2026-11-03</Dt></BookgDt><AcctSvcrRef>B-77</AcctSvcrRef>
<BkTxCd><Domn><Cd>PMNT</Cd><Fmly><Cd>RCDT</Cd><SubFmlyCd>ESCT</SubFmlyCd></Fmly></Domn></BkTxCd>
<NtryDtls><TxDtls><Refs><EndToEndId>NOTPROVIDED</EndToEndId></Refs><RltdPties><Dbtr><Nm>Jane Doe</Nm></Dbtr></RltdPties>
<RmtInf><Strd><CdtrRefInf><Ref>IR-AB12CD</Ref></CdtrRefInf></Strd></RmtInf></TxDtls></NtryDtls></Ntry>
<Ntry><Amt Ccy="EUR">3.50</Amt><CdtDbtInd>DBIT</CdtDbtInd><BookgDt><Dt>2026-11-03</Dt></BookgDt><AcctSvcrRef>F-1</AcctSvcrRef>
<BkTxCd><Domn><Cd>ACMT</Cd><Fmly><Cd>MDOP</Cd><SubFmlyCd>CHRG</SubFmlyCd></Fmly></Domn></BkTxCd></Ntry></Ntfctn></BkToCstmrDbtCdtNtfctn></Document>"#;
        let v = parse_camt054(x).unwrap();
        assert_eq!(v.len(), 1, "the fee debit is not a deposit");
        assert_eq!(v[0].amount_minor, 25_000);
        assert_eq!(v[0].bank_ref, "B-77");
        assert_eq!(v[0].debtor_name, "Jane Doe");
        assert_eq!(v[0].creditor_account, "DE44500105175407324931");
        assert_eq!(v[0].structured_ref.as_deref(), Some("IR-AB12CD"));
        assert_eq!(v[0].ids.end_to_end_id, None);
    }

    #[test]
    fn pacs008_round_trip_and_rejects_unknown_currency() {
        let mut p = sepa_credit();
        p.rail = Rail::Swift;
        p.ccy = Ccy::USD;
        p.ids = TraceIds { uetr: Some("2f0f6c3e-7c1a-4b52-9d6e-0a3c1f2b9e11".into()), end_to_end_id: Some("E2E-1".into()), ..Default::default() };
        p.bank_ref = p.ids.uetr.clone().unwrap();
        p.creditor_account = "8812345678".into();
        let x = write_pacs008("M1", p.booked_at, &p, "DEUTDEFFXXX");
        let got = parse_pacs008(&x).unwrap();
        assert_eq!(got[0].amount_minor, p.amount_minor);
        assert_eq!(got[0].bank_ref, p.bank_ref);
        assert_eq!(got[0].creditor_account, "8812345678");
        assert!(parse_pacs008(&x.replace("Ccy=\"USD\"", "Ccy=\"CHF\"")).is_err());
        assert!(parse_pacs008(&x.replace("<UETR>2f0f6c3e-7c1a-4b52-9d6e-0a3c1f2b9e11</UETR>", "")).is_err());
    }

    #[test]
    fn pacs004_round_trip() {
        let r = PayoutReturn {
            rail: Rail::Sepa,
            bank_ref: "RTR-1".into(),
            original_ref: "po-000017".into(),
            amount_minor: 5000,
            ccy: Ccy::EUR,
            reason_code: "AC04".into(),
            booked_at: ts(2026, 11, 4, 10, 0, 0),
        };
        assert_eq!(parse_pacs004(&write_pacs004("R", r.booked_at, &r)).unwrap(), vec![r]);
    }

    #[test]
    fn camt053_round_trip_keeps_kinds_and_balances() {
        let p = sepa_credit();
        let s = Statement {
            account: "DE44500105175407324931".into(),
            ccy: Ccy::EUR,
            date: ts(2026, 11, 2, 23, 59, 0),
            opening_minor: 100_000,
            closing_minor: 100_000 + p.amount_minor - 350 - 1_000_000,
            entries: vec![
                StatementEntry {
                    bank_ref: p.bank_ref.clone(),
                    amount_minor: p.amount_minor,
                    kind: EntryKind::Credit,
                    booked_at: p.booked_at,
                    our_ref: None,
                    credit: Some(p.clone()),
                },
                StatementEntry { bank_ref: "F-1".into(), amount_minor: -350, kind: EntryKind::Fee, booked_at: p.booked_at, our_ref: None, credit: None },
                StatementEntry {
                    bank_ref: "T-1".into(),
                    amount_minor: -1_000_000,
                    kind: EntryKind::Transfer,
                    booked_at: p.booked_at,
                    our_ref: Some("sweep-1".into()),
                    credit: None,
                },
            ],
        };
        let back = parse_camt053(&write_camt053("S1", &s)).unwrap().remove(0);
        assert_eq!(back.opening_minor, s.opening_minor);
        assert_eq!(back.closing_minor, s.closing_minor);
        assert_eq!(back.entries.iter().map(|e| e.kind.clone()).collect::<Vec<_>>(), vec![EntryKind::Credit, EntryKind::Fee, EntryKind::Transfer]);
        assert_eq!(back.entries[0].credit.as_ref().unwrap(), &p);
        assert_eq!(back.entries[2].our_ref.as_deref(), Some("sweep-1"));
    }
}
