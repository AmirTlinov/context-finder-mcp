use crate::tools::context_doc::ContextDocBuilder;
use crate::tools::util::truncate_to_chars;

const MAX_DOC_LINES: usize = 20;
const MAX_DOC_CHARS: usize = 1200;
const MAX_IMPORTS: usize = 5;
const MAX_TAGS: usize = 6;
const MAX_BUNDLE_TAGS: usize = 6;
const MAX_RELATED_PATHS: usize = 4;

pub(crate) struct HitMeta<'a> {
    pub documentation: Option<&'a str>,
    pub chunk_type: Option<&'a str>,
    pub qualified_name: Option<&'a str>,
    pub parent_scope: Option<&'a str>,
    pub tags: &'a [String],
    pub bundle_tags: &'a [String],
    pub context_imports: &'a [String],
    pub related_paths: &'a [String],
}

pub(crate) fn trim_documentation(doc: Option<&str>) -> Option<String> {
    let doc = doc?;
    let lines: Vec<&str> = doc.lines().collect();
    if lines.is_empty() {
        return None;
    }

    let mut start = 0usize;
    let mut end = lines.len();
    while start < end && lines[start].trim().is_empty() {
        start += 1;
    }
    while end > start && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    if start >= end {
        return None;
    }

    let mut trimmed: Vec<&str> = lines[start..end].to_vec();
    let mut truncated = false;
    if trimmed.len() > MAX_DOC_LINES {
        trimmed.truncate(MAX_DOC_LINES);
        truncated = true;
    }

    let mut out = trimmed.join("\n");
    if out.chars().count() > MAX_DOC_CHARS {
        let max_chars = MAX_DOC_CHARS.saturating_sub(3).max(1);
        out = truncate_to_chars(&out, max_chars);
        truncated = true;
    }
    if truncated {
        out.push_str("...");
    }
    Some(out)
}

pub(crate) fn push_hit_meta(doc: &mut ContextDocBuilder, meta: HitMeta<'_>) {
    let mut meta_parts = Vec::new();
    if let Some(chunk_type) = meta.chunk_type {
        meta_parts.push(format!("type={chunk_type}"));
    }
    if let Some(qualified) = meta.qualified_name {
        meta_parts.push(format!("qual={qualified}"));
    } else if let Some(scope) = meta.parent_scope {
        meta_parts.push(format!("scope={scope}"));
    }
    if !meta_parts.is_empty() {
        doc.push_note(&format!("meta: {}", meta_parts.join(" ")));
    }

    if let Some(tags) = render_list(meta.tags, MAX_TAGS) {
        doc.push_note(&format!("tags: {tags}"));
    }
    if let Some(bundle_tags) = render_list(meta.bundle_tags, MAX_BUNDLE_TAGS) {
        doc.push_note(&format!("bundle: {bundle_tags}"));
    }
    if let Some(imports) = render_list(meta.context_imports, MAX_IMPORTS) {
        doc.push_note(&format!("imports: {imports}"));
    }
    if let Some(paths) = render_list(meta.related_paths, MAX_RELATED_PATHS) {
        doc.push_note(&format!("related_paths: {paths}"));
    }
    if let Some(doc_text) = meta.documentation {
        doc.push_note("doc");
        doc.push_block_smart(doc_text);
    }
}

fn render_list(values: &[String], max: usize) -> Option<String> {
    if values.is_empty() {
        return None;
    }
    let shown: Vec<&str> = values.iter().take(max).map(String::as_str).collect();
    let extra = values.len().saturating_sub(max);
    let mut out = shown.join(", ");
    if extra > 0 {
        out.push_str(&format!(" (+{extra} more)"));
    }
    Some(out)
}
