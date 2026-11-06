//! A small read-only XML tree over quick-xml, enough for ISO 20022 messages, plus an escaper for writers.
//! Namespaces are ignored on purpose: banks send the same message under several schema versions.

use quick_xml::Reader;
use quick_xml::events::Event;

#[derive(Debug, Default, Clone)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }
    pub fn path(&self, path: &[&str]) -> Option<&Node> {
        let mut n = self;
        for p in path {
            n = n.child(p)?;
        }
        Some(n)
    }
    pub fn text_at(&self, path: &[&str]) -> Option<String> {
        self.path(path).map(|n| n.text.trim().to_string()).filter(|s| !s.is_empty())
    }
    /// First of several alternative paths that has text.
    pub fn text_any(&self, paths: &[&[&str]]) -> Option<String> {
        paths.iter().find_map(|p| self.text_at(p))
    }
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug)]
pub struct XmlError(pub String);

impl std::fmt::Display for XmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "xml: {}", self.0)
    }
}
impl std::error::Error for XmlError {}

fn local(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    match s.rsplit_once(':') {
        Some((_, l)) => l.to_string(),
        None => s.to_string(),
    }
}

fn resolve_entity(name: &str) -> Option<String> {
    match name {
        "amp" => Some("&".into()),
        "lt" => Some("<".into()),
        "gt" => Some(">".into()),
        "quot" => Some("\"".into()),
        "apos" => Some("'".into()),
        n if n.starts_with("#x") => u32::from_str_radix(&n[2..], 16).ok().and_then(char::from_u32).map(String::from),
        n if n.starts_with('#') => n[1..].parse::<u32>().ok().and_then(char::from_u32).map(String::from),
        _ => None,
    }
}

pub fn parse(s: &str) -> Result<Node, XmlError> {
    let mut reader = Reader::from_str(s);
    let mut stack: Vec<Node> = vec![Node { name: "#root".into(), ..Default::default() }];
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let mut n = Node { name: local(e.name().as_ref()), ..Default::default() };
                for a in e.attributes().flatten() {
                    n.attrs.push((local(a.key.as_ref()), String::from_utf8_lossy(&a.value).to_string()));
                }
                stack.push(n);
            }
            Ok(Event::Empty(e)) => {
                let mut n = Node { name: local(e.name().as_ref()), ..Default::default() };
                for a in e.attributes().flatten() {
                    n.attrs.push((local(a.key.as_ref()), String::from_utf8_lossy(&a.value).to_string()));
                }
                stack.last_mut().expect("root").children.push(n);
            }
            Ok(Event::Text(t)) => {
                stack.last_mut().expect("root").text.push_str(&String::from_utf8_lossy(&t));
            }
            Ok(Event::CData(t)) => {
                stack.last_mut().expect("root").text.push_str(&String::from_utf8_lossy(&t));
            }
            Ok(Event::GeneralRef(r)) => {
                let name = String::from_utf8_lossy(&r).to_string();
                let v = resolve_entity(&name).ok_or_else(|| XmlError(format!("unknown entity &{name};")))?;
                stack.last_mut().expect("root").text.push_str(&v);
            }
            Ok(Event::End(_)) => {
                let n = stack.pop().ok_or_else(|| XmlError("unbalanced end tag".into()))?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(n),
                    None => return Err(XmlError("unbalanced end tag".into())),
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(XmlError(format!("at byte {}: {e}", reader.buffer_position()))),
        }
    }
    if stack.len() != 1 {
        return Err(XmlError("unclosed element".into()));
    }
    let root = stack.pop().unwrap();
    root.children.into_iter().next().ok_or_else(|| XmlError("empty document".into()))
}

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_namespaced_document_with_entities() {
        let x = r#"<?xml version="1.0"?><Document xmlns="urn:iso"><A><B Ccy="EUR">10.00</B><Nm>M&amp;S &#x26; Co</Nm><E/></A></Document>"#;
        let d = parse(x).unwrap();
        assert_eq!(d.name, "Document");
        assert_eq!(d.text_at(&["A", "B"]).as_deref(), Some("10.00"));
        assert_eq!(d.path(&["A", "B"]).unwrap().attr("Ccy"), Some("EUR"));
        assert_eq!(d.text_at(&["A", "Nm"]).as_deref(), Some("M&S & Co"));
        assert!(d.path(&["A", "E"]).is_some());
    }

    #[test]
    fn rejects_broken_documents() {
        assert!(parse("<A><B></A>").is_err());
        assert!(parse("").is_err());
        assert!(parse("<A>&bogus;</A>").is_err());
    }

    #[test]
    fn escape_round_trips() {
        let s = r#"Müller & Söhne <"GmbH">'"#;
        let d = parse(&format!("<A>{}</A>", esc(s))).unwrap();
        assert_eq!(d.text, s);
    }
}
