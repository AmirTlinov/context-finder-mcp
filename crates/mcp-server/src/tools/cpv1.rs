use std::collections::HashMap;

#[derive(Debug, Clone)]
pub(crate) struct Cpv1EvidencePointer {
    pub(crate) file: String,
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
    pub(crate) source_hash: Option<String>,
}

pub(crate) fn parse_cpv1_dict(pack: &str) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for raw in pack.lines() {
        let line = raw.trim_end_matches('\r');
        if !line.starts_with("D d") {
            continue;
        }
        let mut parts = line.splitn(3, ' ');
        let _ = parts.next(); // "D"
        let Some(id) = parts.next() else { continue };
        let Some(json_str) = parts.next() else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<String>(json_str) else {
            continue;
        };
        out.insert(id.to_string(), value);
    }
    out
}

pub(crate) fn parse_cpv1_evidence(
    pack: &str,
    dict: &HashMap<String, String>,
) -> HashMap<String, Cpv1EvidencePointer> {
    let mut out: HashMap<String, Cpv1EvidencePointer> = HashMap::new();
    for raw in pack.lines() {
        let line = raw.trim_end_matches('\r');
        if !line.starts_with("EV ") {
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 2 {
            continue;
        }
        let ev_id = tokens[1].to_string();
        let mut file: Option<String> = None;
        let mut start_line: Option<usize> = None;
        let mut end_line: Option<usize> = None;
        let mut source_hash: Option<String> = None;

        for &token in &tokens {
            if let Some(d_id) = token.strip_prefix("file=") {
                file = dict.get(d_id).cloned().or_else(|| Some(d_id.to_string()));
                continue;
            }
            if let Some(hash) = token.strip_prefix("sha256=") {
                if !hash.trim().is_empty() {
                    source_hash = Some(hash.to_string());
                }
                continue;
            }
            if token.starts_with('L') && token.contains("-L") {
                if let Some(rest) = token.strip_prefix('L') {
                    if let Some((a, b)) = rest.split_once("-L") {
                        start_line = a.parse::<usize>().ok();
                        end_line = b.parse::<usize>().ok();
                    }
                }
            }
        }

        let (Some(file), Some(start_line), Some(end_line)) = (file, start_line, end_line) else {
            continue;
        };
        out.insert(
            ev_id,
            Cpv1EvidencePointer {
                file,
                start_line,
                end_line,
                source_hash,
            },
        );
    }
    out
}

pub(crate) fn parse_cpv1_anchors(pack: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for raw in pack.lines() {
        let line = raw.trim_end_matches('\r');
        if !line.starts_with("ANCHOR ") {
            continue;
        }
        let mut kind: Option<String> = None;
        let mut ev: Option<String> = None;
        for token in line.split_whitespace() {
            if let Some(v) = token.strip_prefix("kind=") {
                kind = Some(v.to_string());
                continue;
            }
            if let Some(v) = token.strip_prefix("ev=") {
                ev = Some(v.to_string());
                continue;
            }
        }
        let (Some(kind), Some(ev)) = (kind, ev) else {
            continue;
        };
        out.push((kind, ev));
    }
    out
}
