//! Name normalisation and similarity, shared by sender verification and sanctions screening.
//! Deterministic and explainable: every score can be recomputed by hand from the two strings.

use std::collections::HashMap;

/// Legal-form and filler tokens that say nothing about who a party is.
const DROP: &[&str] = &[
    "GMBH",
    "AG",
    "UG",
    "KG",
    "MBH",
    "CO",
    "KGAA",
    "LTD",
    "LIMITED",
    "LLC",
    "LLP",
    "INC",
    "INCORPORATED",
    "CORP",
    "CORPORATION",
    "PLC",
    "SA",
    "SAS",
    "SARL",
    "SL",
    "SRL",
    "SPA",
    "BV",
    "NV",
    "OY",
    "AB",
    "AS",
    "PTE",
    "PTY",
    "THE",
    "AND",
    "OF",
    "DBA",
    "FZE",
    "FZCO",
    "LDA",
];

fn fold_char(c: char, out: &mut String) {
    match c {
        'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'Ā' | 'ā' => out.push('A'),
        'Ç' | 'ç' | 'Č' | 'č' | 'Ć' | 'ć' => out.push('C'),
        'È' | 'É' | 'Ê' | 'Ë' | 'è' | 'é' | 'ê' | 'ë' | 'Ē' | 'ē' | 'Ě' | 'ě' => out.push('E'),
        'Ì' | 'Í' | 'Î' | 'Ï' | 'ì' | 'í' | 'î' | 'ï' | 'İ' | 'ı' => out.push('I'),
        'Ñ' | 'ñ' | 'Ń' | 'ń' => out.push('N'),
        'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' | 'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => out.push('O'),
        'Ù' | 'Ú' | 'Û' | 'Ü' | 'ù' | 'ú' | 'û' | 'ü' | 'Ū' | 'ū' => out.push('U'),
        'Ý' | 'ý' | 'ÿ' => out.push('Y'),
        'Š' | 'š' | 'Ś' | 'ś' | 'Ş' | 'ş' => out.push('S'),
        'Ž' | 'ž' | 'Ź' | 'ź' | 'Ż' | 'ż' => out.push('Z'),
        'Ł' | 'ł' => out.push('L'),
        'Ř' | 'ř' => out.push('R'),
        'Ğ' | 'ğ' => out.push('G'),
        'ß' => out.push_str("SS"),
        'Æ' | 'æ' => out.push_str("AE"),
        'Œ' | 'œ' => out.push_str("OE"),
        c if c.is_ascii_alphanumeric() => out.push(c.to_ascii_uppercase()),
        _ => out.push(' '),
    }
}

#[derive(Clone, Debug, Default)]
pub struct Normaliser {
    /// Spelling variant → canonical token, e.g. MOHAMMED → MUHAMMAD.
    pub variants: HashMap<String, String>,
}

impl Normaliser {
    pub fn new(groups: &[Vec<String>]) -> Self {
        let mut variants = HashMap::new();
        for g in groups {
            if let Some(canon) = g.first() {
                for v in g {
                    variants.insert(v.to_uppercase(), canon.to_uppercase());
                }
            }
        }
        Normaliser { variants }
    }

    pub fn tokens(&self, name: &str) -> Vec<String> {
        let mut s = String::with_capacity(name.len());
        for c in name.chars() {
            fold_char(c, &mut s);
        }
        s.split_whitespace().filter(|t| !DROP.contains(t)).map(|t| self.variants.get(t).cloned().unwrap_or_else(|| t.to_string())).collect()
    }
}

/// Jaro-Winkler similarity in [0, 1].
pub fn jaro_winkler(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut a_m = vec![false; a.len()];
    let mut b_m = vec![false; b.len()];
    let mut matches = 0usize;
    for i in 0..a.len() {
        let lo = i.saturating_sub(window);
        let hi = (i + window + 1).min(b.len());
        for j in lo..hi {
            if !b_m[j] && a[i] == b[j] {
                a_m[i] = true;
                b_m[j] = true;
                matches += 1;
                break;
            }
        }
    }
    if matches == 0 {
        return 0.0;
    }
    let (mut k, mut transpositions) = (0usize, 0usize);
    for i in 0..a.len() {
        if a_m[i] {
            while !b_m[k] {
                k += 1;
            }
            if a[i] != b[k] {
                transpositions += 1;
            }
            k += 1;
        }
    }
    let m = matches as f64;
    let jaro = (m / a.len() as f64 + m / b.len() as f64 + (m - transpositions as f64 / 2.0) / m) / 3.0;
    let prefix = a.iter().zip(&b).take(4).take_while(|(x, y)| x == y).count() as f64;
    jaro + prefix * 0.1 * (1.0 - jaro)
}

/// Two tokens: Jaro-Winkler, discounted when the first letters differ. Spelling variants that change
/// the first letter (YUSUF/JUSUF) are handled by the variants table, so a different initial is
/// usually a different name (ANNA/HANNA).
fn token_similarity(a: &str, b: &str) -> f64 {
    let s = jaro_winkler(a, b);
    if a.chars().next() == b.chars().next() { s } else { s * 0.9 }
}

/// How alike two names are, after normalisation, in [0, 1].
///
/// The score is the higher of (a) Jaro-Winkler on the sorted, joined tokens, which handles word
/// order, and (b) a token-set score: each token of the shorter name against its best partner in the
/// longer one (discounted when the initials differ), lightly penalised per unmatched extra token,
/// which handles missing middle names.
/// A name cut short by a rail's field length (ACH company names hold 16 characters) is scored as a
/// prefix when at least 12 characters survive.
pub fn similarity(n: &Normaliser, a: &str, b: &str) -> f64 {
    let ta = n.tokens(a);
    let tb = n.tokens(b);
    similarity_tokens(&ta, &tb)
}

pub fn similarity_tokens(ta: &[String], tb: &[String]) -> f64 {
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let joined_a: String = ta.concat();
    let joined_b: String = tb.concat();
    if joined_a == joined_b {
        return 1.0;
    }
    let (short, long) = if joined_a.len() <= joined_b.len() { (&joined_a, &joined_b) } else { (&joined_b, &joined_a) };
    if short.len() >= 12 && long.starts_with(short.as_str()) {
        return 0.97;
    }
    let mut sa = ta.to_vec();
    let mut sb = tb.to_vec();
    sa.sort();
    sb.sort();
    let s1 = jaro_winkler(&sa.join(" "), &sb.join(" "));
    let (st, lt) = if ta.len() <= tb.len() { (ta, tb) } else { (tb, ta) };
    let s2 = if st.len() >= 2 {
        let mut used = vec![false; lt.len()];
        let mut total = 0.0;
        for t in st {
            let mut best = (0.0, usize::MAX);
            for (j, u) in lt.iter().enumerate() {
                if used[j] {
                    continue;
                }
                let s = token_similarity(t, u);
                if s > best.0 {
                    best = (s, j);
                }
            }
            if best.1 != usize::MAX {
                used[best.1] = true;
            }
            total += best.0;
        }
        let avg = total / st.len() as f64;
        avg * (1.0 - 0.03 * (lt.len() - st.len()) as f64)
    } else {
        0.0
    };
    s1.max(s2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n() -> Normaliser {
        Normaliser::new(&[vec!["MUHAMMAD".into(), "MOHAMMED".into(), "MOHAMMAD".into(), "MOHAMED".into()]])
    }

    #[test]
    fn jaro_winkler_known_values() {
        assert!((jaro_winkler("MARTHA", "MARHTA") - 0.961).abs() < 0.001);
        assert!((jaro_winkler("DIXON", "DICKSONX") - 0.813).abs() < 0.001);
        assert_eq!(jaro_winkler("", ""), 1.0);
        assert_eq!(jaro_winkler("ABC", ""), 0.0);
    }

    #[test]
    fn normalises_legal_forms_diacritics_and_variants() {
        let n = n();
        assert_eq!(n.tokens("Müller & Söhne GmbH"), vec!["MULLER", "SOHNE"]);
        assert_eq!(n.tokens("Mohammed Al-Rashid"), vec!["MUHAMMAD", "AL", "RASHID"]);
        assert_eq!(similarity(&n, "ACME Holdings Ltd.", "Acme Holdings Limited"), 1.0);
    }

    #[test]
    fn word_order_middle_names_and_truncation() {
        let n = n();
        assert!(similarity(&n, "PETROV Ivan", "Ivan Petrov") > 0.99);
        assert!(similarity(&n, "Ivan Petrov", "Ivan Sergeyevich Petrov") > 0.9);
        assert!(similarity(&n, "NORTHWIND LOGIST", "Northwind Logistics Inc") >= 0.97);
        assert!(similarity(&n, "Mohamed Rashid", "Muhammad Al Rashid") > 0.9);
    }

    #[test]
    fn different_people_stay_apart() {
        let n = n();
        assert!(similarity(&n, "Anna Schmidt", "Hanna Schmitz Consulting") < 0.9);
        assert!(similarity(&n, "Bright Path Logistics", "Northwind Logistics") < 0.85);
        assert!(similarity(&n, "", "Anything") == 0.0);
    }
}
