use serde_json::json;

pub(crate) struct ContextLegend;

impl ContextLegend {
    pub(crate) const TEXT: &'static str = "[LEGEND]\n\
A: answer line (short summary).\n\
R: reference anchor (file:line + optional label); snippet may follow.\n\
If a reference has no snippet, fetch it with file_slice/read_pack.\n\
M: continuation cursor (pass as `cursor`).\n\
N: note/hint (may include score/role/relationship).\n\
Snippets are verbatim project content (source of truth).\n\
Escaping: lines starting with a leading space are quoted.\n\
\n";

    pub(crate) fn structured() -> serde_json::Value {
        json!({
            "content_type": "context",
            "legend": {
                "A": "answer line (short summary)",
                "R": "reference anchor (file:line + optional label); snippet may follow",
                "M": "continuation cursor (pass as `cursor`)",
                "N": "note/hint (may include score/role/relationship)",
                "escaping": "lines starting with a leading space are quoted",
            },
        })
    }
}
