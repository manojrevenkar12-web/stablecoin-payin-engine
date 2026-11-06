//! Fedwire Funds Service messages in tag-value format ({1520}IMAD, {2000}amount, {5000}originator, …).

use crate::domain::{InboundPayment, Rail, TraceIds};
use crate::money::Ccy;
use crate::time::{DAY, HOUR, MINUTE, date_of, days_from_civil};
use anyhow::{Result, anyhow, bail};
use std::collections::BTreeMap;

pub fn tags(s: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let r = rest.strip_prefix('{').ok_or_else(|| anyhow!("expected '{{' at {:?}", &rest[..rest.len().min(12)]))?;
        let close = r.find('}').ok_or_else(|| anyhow!("unterminated tag"))?;
        let tag = &r[..close];
        if tag.len() != 4 || !tag.bytes().all(|b| b.is_ascii_digit()) {
            bail!("bad tag {tag:?}");
        }
        let after = &r[close + 1..];
        let end = after.find('{').unwrap_or(after.len());
        if out.insert(tag.to_string(), after[..end].trim_end().to_string()).is_some() {
            bail!("tag {{{tag}}} repeated");
        }
        rest = &after[end..];
    }
    Ok(out)
}

fn party(v: &str) -> (String, String) {
    // "D<account>*<name>*<address line>*..." — the first letter is the identifier code.
    let mut parts = v.split('*');
    let id = parts.next().unwrap_or("");
    let account = if id.len() > 1 { id[1..].to_string() } else { String::new() };
    let name = parts.next().unwrap_or("").trim().to_string();
    (account, name)
}

pub fn parse(s: &str) -> Result<InboundPayment> {
    let t = tags(s)?;
    let get = |k: &str| t.get(k).cloned().ok_or_else(|| anyhow!("Fedwire message without {{{k}}}"));
    let typ = get("1510")?;
    if typ != "1000" {
        bail!("type/subtype {typ} is not a basic customer transfer");
    }
    let imad = get("1520")?;
    if imad.len() != 22 {
        bail!("IMAD {imad:?} is not 22 characters");
    }
    let amount: i64 = get("2000")?.parse().map_err(|_| anyhow!("bad amount"))?;
    let y: i64 = imad[0..4].parse()?;
    let m: u32 = imad[4..6].parse()?;
    let d: u32 = imad[6..8].parse()?;
    let mut booked = days_from_civil(y, m, d) * DAY;
    if let Some(ts) = t.get("1110") {
        // MMDDHHMM acceptance time
        if ts.len() >= 8 {
            let hh: i64 = ts[4..6].parse().unwrap_or(0);
            let mm: i64 = ts[6..8].parse().unwrap_or(0);
            booked += hh * HOUR + mm * MINUTE;
        }
    }
    let (benef_acct, _) = party(&get("4200")?);
    let (orig_acct, orig_name) = party(&get("5000")?);
    let info = t.get("6000").map(|v| v.split('*').map(str::trim).filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" "));
    Ok(InboundPayment {
        rail: Rail::Fedwire,
        bank_ref: imad.clone(),
        ids: TraceIds { imad: Some(imad), end_to_end_id: t.get("3320").cloned().filter(|s| !s.is_empty()), ..Default::default() },
        amount_minor: amount,
        ccy: Ccy::USD,
        booked_at: booked,
        debtor_name: orig_name,
        debtor_account: orig_acct,
        creditor_account: benef_acct,
        structured_ref: None,
        unstructured: info.filter(|s| !s.is_empty()),
    })
}

/// Wrap text into at most `lines` lines of at most `width` characters, breaking between words like
/// a bank's payment screen does. Words longer than a line are cut.
pub fn wrap_words(text: &str, width: usize, lines: usize) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let mut cur = String::new();
    for w in text.split_whitespace() {
        let mut w = w.to_string();
        while w.chars().count() > width {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            let head: String = w.chars().take(width).collect();
            w = w.chars().skip(width).collect();
            out.push(head);
        }
        if cur.is_empty() {
            cur = w;
        } else if cur.chars().count() + 1 + w.chars().count() <= width {
            cur.push(' ');
            cur.push_str(&w);
        } else {
            out.push(std::mem::replace(&mut cur, w));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.truncate(lines);
    out
}

pub fn write(p: &InboundPayment, sender_aba: &str) -> String {
    let imad = p.ids.imad.clone().unwrap_or_default();
    let (_, m, d) = date_of(p.booked_at);
    let secs = p.booked_at.rem_euclid(DAY);
    let info = wrap_words(&p.unstructured.clone().unwrap_or_default(), 35, 4);
    format!(
        "{{1500}}30            P {{1510}}1000{{1520}}{imad}{{1110}}{m:02}{d:02}{:02}{:02}FT03{{2000}}{:012}{{3100}}{sender_aba}ORIGIN BANK{{3320}}{}{{3400}}021000021PIE BANK NA{{3600}}CTR{{4200}}D{}*MOONPAY ENTERPRISE CLIENT*{{5000}}D{}*{}*{{6000}}{}*",
        secs / HOUR,
        (secs % HOUR) / MINUTE,
        p.amount_minor,
        p.ids.end_to_end_id.clone().unwrap_or_default(),
        p.creditor_account,
        p.debtor_account,
        p.debtor_name,
        info.join("*"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::ts;

    fn wire() -> InboundPayment {
        InboundPayment {
            rail: Rail::Fedwire,
            bank_ref: "20261103QMGFT015000123".into(),
            ids: TraceIds { imad: Some("20261103QMGFT015000123".into()), end_to_end_id: Some("ACME-REF-9".into()), ..Default::default() },
            amount_minor: 2_500_000_00,
            ccy: Ccy::USD,
            booked_at: ts(2026, 11, 3, 15, 42, 0),
            debtor_name: "ACME TREASURY LLC".into(),
            debtor_account: "000123456789".into(),
            creditor_account: "8800009911".into(),
            structured_ref: None,
            unstructured: Some("FOR CREDIT OF IR-Q4ZP1T TREASURY TOP UP NOVEMBER BATCH 2".into()),
        }
    }

    #[test]
    fn round_trip() {
        let p = wire();
        assert_eq!(parse(&write(&p, "026009593")).unwrap(), p);
    }

    #[test]
    fn rejects_bad_messages() {
        let w = write(&wire(), "026009593");
        assert!(parse(&w.replace("{1510}1000", "{1510}1600")).is_err(), "drawdown requests are not deposits");
        assert!(parse(&w.replace("{2000}", "{2001}")).is_err());
        assert!(parse(&format!("{w}{{2000}}000000000001")).is_err(), "repeated tag");
        assert!(parse("not a wire").is_err());
    }
}
