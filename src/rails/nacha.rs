//! NACHA ACH files: 94-character records. The engine reads inbound credits and returns of our own
//! payouts (addenda 99); the simulated bank writes them. Control totals and entry hashes are checked,
//! because a truncated or hand-edited ACH file must be refused, not half-booked.

use crate::domain::{InboundPayment, PayoutReturn, Rail, TraceIds};
use crate::money::Ccy;
use crate::time::{DAY, HOUR, MINUTE, Ts, date_of, days_from_civil};
use anyhow::{Result, anyhow, bail};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AchCredit {
    /// Originator (the sender), from the batch header: 16 characters, so names arrive truncated.
    pub company_name: String,
    pub company_id: String,
    pub sec: String,
    pub entry_description: String,
    pub rdfi_routing: String,
    /// Our account number that was credited.
    pub account: String,
    pub amount_cents: i64,
    pub individual_id: String,
    pub individual_name: String,
    pub trace: String,
    /// Addenda 05 payment-related information, where the sender put the reference.
    pub addenda: Option<String>,
    /// Set on return entries (addenda 99): (reason code, original trace).
    pub return_of: Option<(String, String)>,
}

fn field(line: &str, a: usize, b: usize) -> &str {
    line.get(a..b).unwrap_or("")
}

fn pad(s: &str, n: usize) -> String {
    let mut t: String = s.chars().filter(|c| c.is_ascii() && !c.is_ascii_control()).take(n).collect();
    while t.len() < n {
        t.push(' ');
    }
    t
}

fn num(v: i64, n: usize) -> String {
    format!("{v:0n$}")
}

fn parse_yymmdd(s: &str) -> Option<i64> {
    let y: i64 = s.get(0..2)?.parse().ok()?;
    let m: u32 = s.get(2..4)?.parse().ok()?;
    let d: u32 = s.get(4..6)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(2000 + y, m, d))
}

/// Parse a NACHA file into credit entries plus the file's creation time.
pub fn parse_file(text: &str) -> Result<(Ts, Vec<AchCredit>)> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty() && !l.chars().all(|c| c == '9')).collect();
    if lines.is_empty() {
        bail!("empty ACH file");
    }
    for (i, l) in lines.iter().enumerate() {
        if l.len() != 94 {
            bail!("record {} is {} characters, not 94", i + 1, l.len());
        }
    }
    let header = lines[0];
    if !header.starts_with('1') {
        bail!("file does not start with a file header record");
    }
    let day = parse_yymmdd(field(header, 23, 29)).ok_or_else(|| anyhow!("bad file creation date"))?;
    let hhmm = field(header, 29, 33);
    let hh: i64 = hhmm.get(0..2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let mm: i64 = hhmm.get(2..4).and_then(|s| s.parse().ok()).unwrap_or(0);
    let created = day * DAY + hh * HOUR + mm * MINUTE;

    let mut out: Vec<AchCredit> = vec![];
    let mut batch: Option<(String, String, String, String)> = None;
    let (mut b_hash, mut b_credit, mut b_count) = (0i64, 0i64, 0i64);
    let (mut f_hash, mut f_credit, mut f_count, mut f_batches) = (0i64, 0i64, 0i64, 0i64);
    let mut saw_file_control = false;
    for l in &lines[1..] {
        match &l[0..1] {
            "5" => {
                batch = Some((
                    field(l, 4, 20).trim().to_string(),
                    field(l, 40, 50).trim().to_string(),
                    field(l, 50, 53).to_string(),
                    field(l, 53, 63).trim().to_string(),
                ));
                b_hash = 0;
                b_credit = 0;
                b_count = 0;
            }
            "6" => {
                let (cn, cid, sec, desc) = batch.clone().ok_or_else(|| anyhow!("entry outside a batch"))?;
                let code = field(l, 1, 3);
                let routing = field(l, 3, 11);
                let amount: i64 = field(l, 29, 39).parse().map_err(|_| anyhow!("bad amount in entry {}", field(l, 79, 94)))?;
                b_hash += routing.parse::<i64>().map_err(|_| anyhow!("bad routing number"))?;
                b_count += 1;
                match code {
                    "22" | "32" | "21" | "31" => b_credit += amount,
                    _ => bail!("entry {} is a debit (transaction code {code}); only credits are accepted", field(l, 79, 94)),
                }
                out.push(AchCredit {
                    company_name: cn,
                    company_id: cid,
                    sec,
                    entry_description: desc,
                    rdfi_routing: routing.to_string(),
                    account: field(l, 12, 29).trim().to_string(),
                    amount_cents: amount,
                    individual_id: field(l, 39, 54).trim().to_string(),
                    individual_name: field(l, 54, 76).trim().to_string(),
                    trace: field(l, 79, 94).to_string(),
                    addenda: None,
                    return_of: None,
                });
            }
            "7" => {
                b_count += 1;
                let last = out.last_mut().ok_or_else(|| anyhow!("addenda without an entry"))?;
                match field(l, 1, 3) {
                    "05" => last.addenda = Some(field(l, 3, 83).trim().to_string()),
                    "99" => last.return_of = Some((field(l, 3, 6).to_string(), field(l, 6, 21).trim().to_string())),
                    t => bail!("unsupported addenda type {t}"),
                }
            }
            "8" => {
                let count: i64 = field(l, 4, 10).parse().unwrap_or(-1);
                let hash: i64 = field(l, 10, 20).parse().unwrap_or(-1);
                let credit: i64 = field(l, 32, 44).parse().unwrap_or(-1);
                if count != b_count || hash != b_hash % 10_000_000_000 || credit != b_credit {
                    bail!("batch control totals do not match its entries (count {count} vs {b_count}, credit {credit} vs {b_credit})");
                }
                f_hash += b_hash;
                f_credit += b_credit;
                f_count += b_count;
                f_batches += 1;
                batch = None;
            }
            "9" => {
                saw_file_control = true;
                let batches: i64 = field(l, 1, 7).parse().unwrap_or(-1);
                let count: i64 = field(l, 13, 21).parse().unwrap_or(-1);
                let hash: i64 = field(l, 21, 31).parse().unwrap_or(-1);
                let credit: i64 = field(l, 43, 55).parse().unwrap_or(-1);
                if batches != f_batches || count != f_count || hash != f_hash % 10_000_000_000 || credit != f_credit {
                    bail!("file control totals do not match the batches");
                }
            }
            t => bail!("unknown record type {t}"),
        }
    }
    if batch.is_some() {
        bail!("batch without a batch control record");
    }
    if !saw_file_control {
        bail!("file without a file control record (truncated?)");
    }
    Ok((created, out))
}

/// Inbound credits and payout returns from a parsed file.
pub fn to_engine(created: Ts, entries: &[AchCredit]) -> (Vec<InboundPayment>, Vec<PayoutReturn>) {
    let mut credits = vec![];
    let mut returns = vec![];
    for e in entries {
        if let Some((reason, original)) = &e.return_of {
            returns.push(PayoutReturn {
                rail: Rail::Ach,
                bank_ref: e.trace.clone(),
                original_ref: original.clone(),
                amount_minor: e.amount_cents,
                ccy: Ccy::USD,
                reason_code: reason.clone(),
                booked_at: created,
            });
            continue;
        }
        credits.push(InboundPayment {
            rail: Rail::Ach,
            bank_ref: e.trace.clone(),
            ids: TraceIds { ach_trace: Some(e.trace.clone()), ..Default::default() },
            amount_minor: e.amount_cents,
            ccy: Ccy::USD,
            booked_at: created,
            debtor_name: e.company_name.clone(),
            debtor_account: e.company_id.clone(),
            creditor_account: e.account.clone(),
            structured_ref: if e.individual_id.is_empty() { None } else { Some(e.individual_id.clone()) },
            unstructured: e.addenda.clone(),
        });
    }
    (credits, returns)
}

pub fn parse(text: &str) -> Result<(Vec<InboundPayment>, Vec<PayoutReturn>)> {
    let (created, entries) = parse_file(text)?;
    Ok(to_engine(created, &entries))
}

/// Write one file with one batch per (company name, company id) group, in input order.
pub fn write_file(created: Ts, origin_name: &str, entries: &[AchCredit]) -> String {
    let (y, m, d) = date_of(created);
    let hh = (created.rem_euclid(DAY)) / HOUR;
    let mi = (created.rem_euclid(DAY) % HOUR) / MINUTE;
    let yymmdd = format!("{:02}{:02}{:02}", y % 100, m, d);
    let mut lines =
        vec![format!("101 091000019 {}{yymmdd}{hh:02}{mi:02}A094101{}{}{}", pad("021000021", 9), pad("PIE BANK NA", 23), pad(origin_name, 23), pad("", 8))];
    let mut groups: Vec<(String, String, Vec<&AchCredit>)> = vec![];
    for e in entries {
        match groups.iter_mut().find(|(n, i, _)| *n == e.company_name && *i == e.company_id) {
            Some(g) => g.2.push(e),
            None => groups.push((e.company_name.clone(), e.company_id.clone(), vec![e])),
        }
    }
    let (mut f_hash, mut f_credit, mut f_count) = (0i64, 0i64, 0i64);
    for (bno, (name, id, es)) in groups.iter().enumerate() {
        let first = es[0];
        lines.push(format!(
            "5220{}{}{}{}{}{yymmdd}{yymmdd}   1{}{}",
            pad(name, 16),
            pad("", 20),
            pad(id, 10),
            pad(&first.sec, 3),
            pad(&first.entry_description, 10),
            pad(&first.trace[..8.min(first.trace.len())], 8),
            num(bno as i64 + 1, 7)
        ));
        let (mut b_hash, mut b_credit, mut b_count) = (0i64, 0i64, 0i64);
        for e in es {
            let has_addenda = e.addenda.is_some() || e.return_of.is_some();
            let code = if e.return_of.is_some() { "21" } else { "22" };
            lines.push(format!(
                "6{code}{}{}{}{}{}{}  {}{}",
                pad(&e.rdfi_routing, 8),
                "1",
                pad(&e.account, 17),
                num(e.amount_cents, 10),
                pad(&e.individual_id, 15),
                pad(&e.individual_name, 22),
                if has_addenda { "1" } else { "0" },
                pad(&e.trace, 15)
            ));
            b_hash += e.rdfi_routing.parse::<i64>().unwrap_or(0);
            b_credit += e.amount_cents;
            b_count += 1;
            if let Some((reason, original)) = &e.return_of {
                lines.push(format!("799{}{}{}{}{}{}", pad(reason, 3), pad(original, 15), pad("", 6), pad(&e.rdfi_routing, 8), pad("", 44), pad(&e.trace, 15)));
                b_count += 1;
            } else if let Some(a) = &e.addenda {
                lines.push(format!("705{}0001{}", pad(a, 80), pad(&e.trace[e.trace.len().saturating_sub(7)..], 7)));
                b_count += 1;
            }
        }
        lines.push(format!(
            "8220{}{}{}{}{}{}{}{}{}",
            num(b_count, 6),
            num(b_hash % 10_000_000_000, 10),
            num(0, 12),
            num(b_credit, 12),
            pad(id, 10),
            pad("", 19),
            pad("", 6),
            pad(&first.trace[..8.min(first.trace.len())], 8),
            num(bno as i64 + 1, 7)
        ));
        f_hash += b_hash;
        f_credit += b_credit;
        f_count += b_count;
    }
    let records = lines.len() + 1;
    let blocks = records.div_ceil(10);
    lines.push(format!(
        "9{}{}{}{}{}{}{}",
        num(groups.len() as i64, 6),
        num(blocks as i64, 6),
        num(f_count, 8),
        num(f_hash % 10_000_000_000, 10),
        num(0, 12),
        num(f_credit, 12),
        pad("", 39)
    ));
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::ts;

    fn entry(trace: &str, amount: i64) -> AchCredit {
        AchCredit {
            company_name: "NORTHWIND LOGISTICS INC".into(),
            company_id: "1234567890".into(),
            sec: "CCD".into(),
            entry_description: "PAYMENT".into(),
            rdfi_routing: "02100002".into(),
            account: "8800001234".into(),
            amount_cents: amount,
            individual_id: "IR-7KQ2MX".into(),
            individual_name: "NORTHWIND LOGISTICS".into(),
            trace: trace.into(),
            addenda: Some("INV 4411 IR-7KQ2MX".into()),
            return_of: None,
        }
    }

    #[test]
    fn round_trip_and_truncation() {
        let created = ts(2026, 11, 3, 14, 5, 0);
        let f = write_file(created, "ORIGIN BANK", &[entry("021000020000001", 150_000), entry("021000020000002", 99)]);
        for l in f.lines() {
            assert_eq!(l.len(), 94, "{l:?}");
        }
        let (c, got) = parse_file(&f).unwrap();
        assert_eq!(c, created);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].company_name, "NORTHWIND LOGIST", "the batch header holds 16 characters");
        assert_eq!(got[0].amount_cents, 150_000);
        assert_eq!(got[1].addenda.as_deref(), Some("INV 4411 IR-7KQ2MX"));
        let (credits, returns) = to_engine(c, &got);
        assert_eq!(credits.len(), 2);
        assert!(returns.is_empty());
        assert_eq!(credits[0].bank_ref, "021000020000001");
    }

    #[test]
    fn returns_are_read_as_returns() {
        let mut r = entry("091000010000077", 25_000);
        r.addenda = None;
        r.return_of = Some(("R03".into(), "026009590000012".into()));
        let f = write_file(ts(2026, 11, 4, 9, 0, 0), "ORIGIN BANK", &[r]);
        let (credits, returns) = parse(&f).unwrap();
        assert!(credits.is_empty());
        assert_eq!(returns[0].original_ref, "026009590000012");
        assert_eq!(returns[0].reason_code, "R03");
    }

    #[test]
    fn refuses_tampered_or_truncated_files() {
        let f = write_file(ts(2026, 11, 3, 14, 5, 0), "ORIGIN BANK", &[entry("021000020000001", 150_000)]);
        // Change the amount but not the control totals.
        let tampered = f.replacen("0000150000", "0000950000", 1);
        assert!(parse_file(&tampered).unwrap_err().to_string().contains("control totals"));
        // Drop the file control record.
        let truncated: String = f.lines().filter(|l| !l.starts_with('9')).map(|l| format!("{l}\n")).collect();
        assert!(parse_file(&truncated).unwrap_err().to_string().contains("truncated"));
        // A short line.
        assert!(parse_file(&f.replacen("PAYMENT", "PAY", 1)).is_err());
    }
}
