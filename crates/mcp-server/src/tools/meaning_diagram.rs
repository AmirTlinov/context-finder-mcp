use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Root,
    Focus,
    Anchor,
    Symbol,
    Boundary,
    Entrypoint,
    Contract,
    Broker,
    Channel,
}

impl NodeKind {
    fn css_class(self) -> &'static str {
        match self {
            NodeKind::Root => "root",
            NodeKind::Focus => "focus",
            NodeKind::Anchor => "anchor",
            NodeKind::Symbol => "symbol",
            NodeKind::Boundary => "boundary",
            NodeKind::Entrypoint => "entrypoint",
            NodeKind::Contract => "contract",
            NodeKind::Broker => "broker",
            NodeKind::Channel => "channel",
        }
    }
}

#[derive(Debug, Clone)]
struct DiagramNode {
    id: String,
    kind: NodeKind,
    title: String,
    subtitle: Option<String>,
    ev: Option<String>,
}

#[derive(Debug, Clone)]
struct DiagramEdge {
    from: String,
    to: String,
    kind: &'static str,
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn truncate_middle(text: &str, max_len: usize) -> String {
    if max_len < 8 || text.chars().count() <= max_len {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_len {
        return text.to_string();
    }
    let keep = max_len.saturating_sub(3);
    let left = keep / 2;
    let right = keep.saturating_sub(left);
    let mut out = String::new();
    out.extend(chars.iter().take(left));
    out.push_str("...");
    out.extend(chars.iter().skip(chars.len().saturating_sub(right)));
    out
}

fn parse_cpv1_dict(pack: &str) -> HashMap<String, String> {
    let mut dict: HashMap<String, String> = HashMap::new();
    for line in pack.lines() {
        if !line.starts_with("D ") {
            continue;
        }
        let mut parts = line.splitn(3, ' ');
        let _ = parts.next();
        let Some(id) = parts.next() else {
            continue;
        };
        let Some(json) = parts.next() else {
            continue;
        };
        let parsed: Result<String, _> = serde_json::from_str(json);
        if let Ok(value) = parsed {
            dict.insert(id.to_string(), value);
        }
    }
    dict
}

fn token_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(key))
}

fn dict_lookup(dict: &HashMap<String, String>, id: &str) -> String {
    dict.get(id).cloned().unwrap_or_else(|| id.to_string())
}

pub(super) fn render_meaning_pack_svg(pack: &str, query: &str, max_nodes: usize) -> String {
    let dict = parse_cpv1_dict(pack);

    let mut focus: Option<DiagramNode> = None;
    let mut anchors: Vec<DiagramNode> = Vec::new();
    let mut symbols: Vec<DiagramNode> = Vec::new();
    let mut interfaces: Vec<DiagramNode> = Vec::new();
    let mut flows: Vec<(String, String)> = Vec::new();

    // Use stable IDs based on insertion order.
    let mut next_anchor_idx = 0usize;
    let mut next_symbol_idx = 0usize;
    let mut next_iface_idx = 0usize;
    let mut channel_ids: BTreeMap<String, String> = BTreeMap::new();

    for line in pack.lines() {
        if line.starts_with("FOCUS ") {
            // Focus lines are only present in meaning_focus CPV1, but we keep the renderer generic.
            let dir = token_value(line, "dir=").unwrap_or("d?");
            let file = token_value(line, "file=").unwrap_or("d?");
            let dir = dict_lookup(&dict, dir);
            let file = dict_lookup(&dict, file);

            // Prefer the file path if present; fall back to dir.
            let subtitle = if !file.trim().is_empty() && file != "d?" {
                Some(file)
            } else {
                Some(dir)
            };

            focus = Some(DiagramNode {
                id: "f0".to_string(),
                kind: NodeKind::Focus,
                title: "focus".to_string(),
                subtitle,
                ev: None,
            });
            continue;
        }

        if line.starts_with("ANCHOR ") {
            let kind = token_value(line, "kind=").unwrap_or("anchor");
            let label = token_value(line, "label=").unwrap_or("d?");
            let file = token_value(line, "file=").unwrap_or("d?");
            let ev = token_value(line, "ev=").map(str::to_string);
            let label = dict_lookup(&dict, label);
            let file = dict_lookup(&dict, file);

            let id = format!("a{next_anchor_idx}");
            next_anchor_idx += 1;
            anchors.push(DiagramNode {
                id,
                kind: NodeKind::Anchor,
                title: format!("{kind}: {label}"),
                subtitle: Some(file),
                ev,
            });
            continue;
        }

        if line.starts_with("SYM ") {
            // Symbols are hints, not evidence-backed claims (no EV).
            let kind = token_value(line, "kind=").unwrap_or("sym");
            let name = token_value(line, "name=").unwrap_or("d?");
            let name = dict_lookup(&dict, name);

            let span = line
                .split_whitespace()
                .find(|t| t.starts_with('L') && t.contains("-L"))
                .unwrap_or("")
                .to_string();
            let subtitle = (!span.is_empty()).then_some(span);

            let id = format!("s{next_symbol_idx}");
            next_symbol_idx += 1;
            symbols.push(DiagramNode {
                id,
                kind: NodeKind::Symbol,
                title: format!("{kind}: {name}"),
                subtitle,
                ev: None,
            });
            continue;
        }

        if line.starts_with("BOUNDARY ") {
            let kind = token_value(line, "kind=").unwrap_or("boundary");
            let file = token_value(line, "file=").unwrap_or("d?");
            let ev = token_value(line, "ev=").map(str::to_string);
            let file = dict_lookup(&dict, file);

            let id = format!("i{next_iface_idx}");
            next_iface_idx += 1;
            interfaces.push(DiagramNode {
                id,
                kind: NodeKind::Boundary,
                title: format!("boundary: {kind}"),
                subtitle: Some(file),
                ev,
            });
            continue;
        }

        if line.starts_with("ENTRY ") {
            let file = token_value(line, "file=").unwrap_or("d?");
            let ev = token_value(line, "ev=").map(str::to_string);
            let file = dict_lookup(&dict, file);

            let id = format!("i{next_iface_idx}");
            next_iface_idx += 1;
            interfaces.push(DiagramNode {
                id,
                kind: NodeKind::Entrypoint,
                title: "entrypoint".to_string(),
                subtitle: Some(file),
                ev,
            });
            continue;
        }

        if line.starts_with("CONTRACT ") {
            let kind = token_value(line, "kind=").unwrap_or("contract");
            let file = token_value(line, "file=").unwrap_or("d?");
            let ev = token_value(line, "ev=").map(str::to_string);
            let file = dict_lookup(&dict, file);

            let id = format!("i{next_iface_idx}");
            next_iface_idx += 1;
            interfaces.push(DiagramNode {
                id,
                kind: NodeKind::Contract,
                title: format!("contract: {kind}"),
                subtitle: Some(file),
                ev,
            });
            continue;
        }

        if line.starts_with("BROKER ") {
            let proto = token_value(line, "proto=").unwrap_or("broker");
            let file = token_value(line, "file=").unwrap_or("d?");
            let ev = token_value(line, "ev=").map(str::to_string);
            let file = dict_lookup(&dict, file);

            let id = format!("i{next_iface_idx}");
            next_iface_idx += 1;
            interfaces.push(DiagramNode {
                id,
                kind: NodeKind::Broker,
                title: format!("broker: {proto}"),
                subtitle: Some(file),
                ev,
            });
            continue;
        }

        if line.starts_with("FLOW ") {
            let contract = token_value(line, "contract=").unwrap_or("d?");
            let chan = token_value(line, "chan=").unwrap_or("d?");
            let contract = dict_lookup(&dict, contract);
            let chan = dict_lookup(&dict, chan);
            flows.push((contract, chan));
            continue;
        }
    }

    // Budget nodes: anchors first, then interfaces, then channels (only when referenced by flows).
    let mut nodes: Vec<DiagramNode> = Vec::new();
    let mut edges: Vec<DiagramEdge> = Vec::new();

    let root_id = "root".to_string();
    nodes.push(DiagramNode {
        id: root_id.clone(),
        kind: NodeKind::Root,
        title: "repo".to_string(),
        subtitle: None,
        ev: None,
    });

    let mut remaining = max_nodes.max(3).saturating_sub(1);

    let focus_id = focus.as_ref().map(|n| n.id.clone());
    if remaining > 0 {
        if let Some(focus) = focus.take() {
            remaining = remaining.saturating_sub(1);
            edges.push(DiagramEdge {
                from: root_id.clone(),
                to: focus.id.clone(),
                kind: "focus",
            });
            nodes.push(focus);
        }
    }

    let anchors_take = anchors.len().min(remaining);
    for anchor in anchors.into_iter().take(anchors_take) {
        remaining = remaining.saturating_sub(1);
        edges.push(DiagramEdge {
            from: root_id.clone(),
            to: anchor.id.clone(),
            kind: "root",
        });
        nodes.push(anchor);
    }

    let symbols_take = symbols.len().min(remaining);
    for sym in symbols.into_iter().take(symbols_take) {
        remaining = remaining.saturating_sub(1);
        edges.push(DiagramEdge {
            from: focus_id.clone().unwrap_or_else(|| root_id.clone()),
            to: sym.id.clone(),
            kind: "focus",
        });
        nodes.push(sym);
    }

    let interfaces_take = interfaces.len().min(remaining);
    for iface in interfaces.into_iter().take(interfaces_take) {
        remaining = remaining.saturating_sub(1);
        edges.push(DiagramEdge {
            from: root_id.clone(),
            to: iface.id.clone(),
            kind: "root",
        });
        nodes.push(iface);
    }

    // Materialize up to a few channel nodes, but only if we still have budget.
    if remaining > 0 {
        let mut seen_channels: HashSet<String> = HashSet::new();
        for (_, chan) in &flows {
            if remaining == 0 {
                break;
            }
            if !seen_channels.insert(chan.clone()) {
                continue;
            }
            let next_id = format!("c{}", channel_ids.len());
            let id = channel_ids
                .entry(chan.clone())
                .or_insert_with(|| next_id.clone())
                .clone();
            nodes.push(DiagramNode {
                id: id.clone(),
                kind: NodeKind::Channel,
                title: "channel".to_string(),
                subtitle: Some(chan.clone()),
                ev: None,
            });
            edges.push(DiagramEdge {
                from: root_id.clone(),
                to: id,
                kind: "root",
            });
            remaining = remaining.saturating_sub(1);
        }
    }

    // Optional: connect contract nodes to channel nodes when both are present.
    let mut node_by_subtitle: HashMap<String, String> = HashMap::new();
    for node in &nodes {
        if let Some(sub) = node.subtitle.as_deref() {
            node_by_subtitle.insert(sub.to_string(), node.id.clone());
        }
    }
    let mut seen_flow_edges: HashSet<(String, String)> = HashSet::new();
    for (contract, chan) in flows {
        let Some(from_id) = node_by_subtitle.get(&contract).cloned() else {
            continue;
        };
        let Some(to_id) = node_by_subtitle.get(&chan).cloned() else {
            continue;
        };
        if !seen_flow_edges.insert((from_id.clone(), to_id.clone())) {
            continue;
        }
        edges.push(DiagramEdge {
            from: from_id,
            to: to_id,
            kind: "flow",
        });
    }

    render_svg(nodes, edges, query)
}

fn render_svg(nodes: Vec<DiagramNode>, edges: Vec<DiagramEdge>, query: &str) -> String {
    const NODE_W: i32 = 300;
    const NODE_H: i32 = 70;
    const ROOT_W: i32 = 260;
    const ROOT_H: i32 = 70;
    const ROW_SPACING: i32 = 90;
    const TOP: i32 = 70;

    const ANCHOR_X: i32 = 40;
    const ROOT_X: i32 = 380;
    const IFACE_X: i32 = 720;

    let mut anchor_nodes = Vec::new();
    let mut iface_nodes = Vec::new();
    let mut other_nodes = Vec::new();
    for n in nodes {
        match n.kind {
            NodeKind::Anchor | NodeKind::Focus => anchor_nodes.push(n),
            NodeKind::Root => other_nodes.push(n),
            _ => iface_nodes.push(n),
        }
    }

    let anchors_rows = anchor_nodes.len().max(1) as i32;
    let iface_rows = iface_nodes.len().max(1) as i32;
    let rows = anchors_rows.max(iface_rows);
    let height = TOP + rows * ROW_SPACING + 60;
    let width = 1060;

    let root_center_y = TOP + ((rows - 1) * ROW_SPACING) / 2;

    // Stable lookup: id -> (center_x, center_y, w, h, kind).
    let mut pos: HashMap<String, (i32, i32, i32, i32, NodeKind)> = HashMap::new();

    for (idx, node) in anchor_nodes.iter().enumerate() {
        let cy = TOP + (idx as i32) * ROW_SPACING;
        pos.insert(
            node.id.clone(),
            (ANCHOR_X + NODE_W / 2, cy, NODE_W, NODE_H, node.kind),
        );
    }

    // Root is always present.
    pos.insert(
        "root".to_string(),
        (
            ROOT_X + ROOT_W / 2,
            root_center_y,
            ROOT_W,
            ROOT_H,
            NodeKind::Root,
        ),
    );

    for (idx, node) in iface_nodes.iter().enumerate() {
        let cy = TOP + (idx as i32) * ROW_SPACING;
        pos.insert(
            node.id.clone(),
            (IFACE_X + NODE_W / 2, cy, NODE_W, NODE_H, node.kind),
        );
    }

    let query = truncate_middle(query.trim(), 96);
    let query = xml_escape(&query);

    let mut svg = String::new();
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\">"
    ));
    svg.push_str(
        "<defs>\
            <marker id=\"arrow\" markerWidth=\"10\" markerHeight=\"7\" refX=\"10\" refY=\"3.5\" orient=\"auto\">\
              <polygon points=\"0 0, 10 3.5, 0 7\" fill=\"#64748b\"/>\
            </marker>\
          </defs>",
    );
    svg.push_str(
        "<style>\
          .title{font:600 16px system-ui, -apple-system, Segoe UI, Roboto, sans-serif; fill:#0f172a}\
          .subtitle{font:400 12px system-ui, -apple-system, Segoe UI, Roboto, sans-serif; fill:#475569}\
          .node{stroke:#334155; stroke-width:1; rx:12; ry:12}\
          .node.root{fill:#e2e8f0}\
          .node.focus{fill:#e0f2fe}\
          .node.anchor{fill:#eef2ff}\
          .node.symbol{fill:#f0fdf4}\
          .node.entrypoint{fill:#f1f5f9}\
          .node.contract{fill:#fff7ed}\
          .node.boundary{fill:#fef2f2}\
          .node.broker{fill:#fffbeb}\
          .node.channel{fill:#ecfeff}\
          .node.other{fill:#f8fafc}\
          .label{font:600 13px system-ui, -apple-system, Segoe UI, Roboto, sans-serif; fill:#0f172a}\
          .small{font:400 11px ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace; fill:#334155}\
          .edge{stroke:#64748b; stroke-width:1.2}\
          .edge.focus{stroke:#0ea5e9; stroke-width:1.4}\
          .edge.flow{stroke:#2563eb; stroke-width:1.4}\
        </style>",
    );

    svg.push_str("<text x=\"40\" y=\"28\" class=\"title\">Meaning Graph</text>");
    svg.push_str(&format!(
        "<text x=\"40\" y=\"48\" class=\"subtitle\">query: {query}</text>"
    ));

    // Edges first (under nodes).
    for edge in &edges {
        let Some(&(from_cx, from_cy, from_w, _, _)) = pos.get(&edge.from) else {
            continue;
        };
        let Some(&(to_cx, to_cy, to_w, _, _)) = pos.get(&edge.to) else {
            continue;
        };

        let (x1, y1, x2, y2) = if from_cx < to_cx {
            (from_cx + from_w / 2, from_cy, to_cx - to_w / 2, to_cy)
        } else {
            (from_cx - from_w / 2, from_cy, to_cx + to_w / 2, to_cy)
        };

        let class = if edge.kind == "flow" {
            "edge flow"
        } else if edge.kind == "focus" {
            "edge focus"
        } else {
            "edge"
        };
        svg.push_str(&format!(
            "<line x1=\"{x1}\" y1=\"{y1}\" x2=\"{x2}\" y2=\"{y2}\" class=\"{class}\" marker-end=\"url(#arrow)\"/>"
        ));
    }

    // Nodes.
    for node in other_nodes
        .iter()
        .chain(anchor_nodes.iter())
        .chain(iface_nodes.iter())
    {
        let Some(&(cx, cy, w, h, kind)) = pos.get(&node.id) else {
            continue;
        };
        let x = cx - w / 2;
        let y = cy - h / 2;

        let (title, subtitle) = match kind {
            NodeKind::Root => ("repo".to_string(), None),
            _ => (node.title.clone(), node.subtitle.clone()),
        };

        let title = truncate_middle(title.trim(), 40);
        let title = xml_escape(&title);
        let subtitle = subtitle.map(|s| xml_escape(&truncate_middle(s.trim(), 46)));
        let ev = node.ev.clone().unwrap_or_default();
        let ev = xml_escape(ev.trim());

        let tooltip = if ev.is_empty() {
            title.clone()
        } else {
            format!("{title} ({ev})")
        };

        svg.push_str("<g>");
        svg.push_str(&format!("<title>{}</title>", tooltip));
        svg.push_str(&format!(
            "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" class=\"node {}\"/>",
            kind.css_class()
        ));
        svg.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" class=\"label\">{}</text>",
            x + 14,
            y + 28,
            title
        ));
        if let Some(sub) = subtitle {
            svg.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" class=\"small\">{}</text>",
                x + 14,
                y + 50,
                sub
            ));
        }
        if !ev.is_empty() {
            svg.push_str(&format!(
                "<text x=\"{}\" y=\"{}\" class=\"small\">{}</text>",
                x + w - 14,
                y + 18,
                ev
            ));
        }
        svg.push_str("</g>");
    }

    svg.push_str("</svg>");
    // Ensure the SVG is valid UTF-8 and "image-like" for tools that sanity-check payloads.
    if serde_json::from_str::<Value>(&format!("\"{}\"", svg_escape_for_json_smoke(&svg))).is_err() {
        return "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"600\" height=\"120\"><text x=\"20\" y=\"60\">diagram unavailable</text></svg>".to_string();
    }
    svg
}

fn svg_escape_for_json_smoke(svg: &str) -> String {
    // We use a cheap JSON string roundtrip to avoid returning invalid UTF-8 / control characters.
    // This is not a general-purpose sanitizer; SVG is still treated as trusted internal output.
    svg.replace('\\', "\\\\").replace('"', "\\\"")
}
