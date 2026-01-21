use anyhow::{Context as AnyhowContext, Result};
use context_indexer::{FileScanner, ToolMeta};
use regex::Regex;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use super::cursor::{cursor_fingerprint, encode_cursor, CURSOR_VERSION};
use super::paths::normalize_relative_path;
use super::schemas::content_format::ContentFormat;
use super::schemas::grep_context::{
    GrepContextCursorV1, GrepContextHunk, GrepContextRequest, GrepContextResult,
    GrepContextTruncation,
};
use super::secrets::is_potential_secret_path;
use super::ContextFinderService;

#[derive(Debug, Clone)]
struct GrepRange {
    start_line: usize,
    end_line: usize,
    match_lines: Vec<usize>,
}

fn merge_grep_ranges(mut ranges: Vec<GrepRange>) -> Vec<GrepRange> {
    ranges.sort_by(|a, b| {
        a.start_line
            .cmp(&b.start_line)
            .then_with(|| a.end_line.cmp(&b.end_line))
    });

    let mut merged: Vec<GrepRange> = Vec::new();
    for range in ranges {
        let Some(last) = merged.last_mut() else {
            merged.push(range);
            continue;
        };

        if range.start_line <= last.end_line.saturating_add(1) {
            last.end_line = last.end_line.max(range.end_line);
            last.match_lines.extend(range.match_lines);
            continue;
        }

        merged.push(range);
    }

    for range in &mut merged {
        range.match_lines.sort_unstable();
        range.match_lines.dedup();
    }

    merged
}

pub(super) struct GrepContextComputeOptions<'a> {
    pub(super) case_sensitive: bool,
    pub(super) before: usize,
    pub(super) after: usize,
    pub(super) max_matches: usize,
    pub(super) max_hunks: usize,
    /// Output budget for the full tool response (cursor uses this value).
    pub(super) max_chars: usize,
    /// Internal content budget (hunk text), leaving headroom for the JSON envelope.
    pub(super) content_max_chars: usize,
    pub(super) resume_file: Option<&'a str>,
    pub(super) resume_line: usize,
}

#[derive(Debug)]
struct MatchScanResult {
    match_lines: Vec<usize>,
    hit_match_limit: bool,
}

#[derive(Debug)]
struct GrepContextAccumulators {
    hunks: Vec<GrepContextHunk>,
    used_chars: usize,
    truncated: bool,
    truncation: Option<GrepContextTruncation>,
    scanned_files: usize,
    matched_files: usize,
    returned_matches: usize,
    total_matches: usize,
    next_cursor_state: Option<(String, usize)>,
}

impl GrepContextAccumulators {
    const fn new() -> Self {
        Self {
            hunks: Vec::new(),
            used_chars: 0,
            truncated: false,
            truncation: None,
            scanned_files: 0,
            matched_files: 0,
            returned_matches: 0,
            total_matches: 0,
            next_cursor_state: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct GrepHunkBuildOptions {
    file_resume_line: usize,
    max_hunks: usize,
    max_chars: usize,
    format: ContentFormat,
}

#[derive(Debug, Clone, Copy)]
struct GrepCursorOptions<'a> {
    file_pattern: Option<&'a str>,
    case_sensitive: bool,
    before: usize,
    after: usize,
    max_matches: usize,
    max_hunks: usize,
    max_chars: usize,
}

fn canonicalize_request_file(root: &Path, file: &str) -> Result<(String, PathBuf)> {
    let canonical = root
        .join(Path::new(file))
        .canonicalize()
        .with_context(|| format!("Invalid file '{file}'"))?;
    if !canonical.starts_with(root) {
        anyhow::bail!("File '{file}' is outside project root");
    }

    let display = normalize_relative_path(root, &canonical)
        .unwrap_or_else(|| canonical.to_string_lossy().into_owned().replace('\\', "/"));
    Ok((display, canonical))
}

async fn collect_candidates(
    root: &Path,
    request: &GrepContextRequest,
    file_pattern: Option<&str>,
) -> Result<(String, Vec<(String, PathBuf)>)> {
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    let allow_secrets = request.allow_secrets.unwrap_or(false);

    if let Some(file) = request
        .file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !allow_secrets && is_potential_secret_path(file) {
            anyhow::bail!("Refusing to search potential secret file");
        }
        let (display, canonical) = canonicalize_request_file(root, file)?;
        candidates.push((display, canonical));
        return Ok(("filesystem".to_string(), candidates));
    }

    let scanner = FileScanner::new(root);
    let files = scanner.scan();
    let mut rels: Vec<String> = files
        .into_iter()
        .filter_map(|p| normalize_relative_path(root, &p))
        .collect();
    rels.sort();
    for rel in rels {
        if !allow_secrets && is_potential_secret_path(&rel) {
            continue;
        }
        if !ContextFinderService::matches_file_pattern(&rel, file_pattern) {
            continue;
        }
        candidates.push((rel.clone(), root.join(&rel)));
    }

    Ok(("filesystem".to_string(), candidates))
}

fn ensure_resume_file_exists(
    resume_file: Option<&str>,
    candidates: &[(String, PathBuf)],
) -> Result<()> {
    let Some(resume_file) = resume_file else {
        return Ok(());
    };

    if candidates.iter().any(|(file, _)| file == resume_file) {
        Ok(())
    } else {
        anyhow::bail!("Cursor resume_file not found: {resume_file}");
    }
}

fn trimmed_non_empty_str(input: Option<&str>) -> Option<&str> {
    input.map(str::trim).filter(|value| !value.is_empty())
}

fn file_resume_line(display_file: &str, resume_file: Option<&str>, resume_line: usize) -> usize {
    if Some(display_file) == resume_file {
        resume_line
    } else {
        1
    }
}

fn scan_match_lines_for_file(
    file_path: &Path,
    regex: &Regex,
    file_resume_line: usize,
    max_matches: usize,
    total_matches: &mut usize,
) -> std::result::Result<MatchScanResult, std::io::Error> {
    let file = std::fs::File::open(file_path)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut line_no = 0usize;
    let mut match_lines: Vec<usize> = Vec::new();
    let mut hit_match_limit = false;

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            break;
        }
        line_no += 1;

        let text = line.trim_end_matches(&['\r', '\n'][..]);
        if !regex.is_match(text) {
            continue;
        }
        match_lines.push(line_no);
        if line_no >= file_resume_line {
            *total_matches += 1;
            if *total_matches >= max_matches {
                hit_match_limit = true;
                break;
            }
        }
    }

    Ok(MatchScanResult {
        match_lines,
        hit_match_limit,
    })
}

fn build_ranges_from_matches(match_lines: &[usize], before: usize, after: usize) -> Vec<GrepRange> {
    let ranges: Vec<GrepRange> = match_lines
        .iter()
        .map(|&ln| {
            let start_line = ln.saturating_sub(before).max(1);
            let end_line = ln.saturating_add(after);
            GrepRange {
                start_line,
                end_line,
                match_lines: vec![ln],
            }
        })
        .collect();

    merge_grep_ranges(ranges)
}

fn build_hunks_for_file(
    acc: &mut GrepContextAccumulators,
    display_file: String,
    file_path: &Path,
    ranges: &[GrepRange],
    opts: GrepHunkBuildOptions,
) -> bool {
    fn truncate_to_chars(input: &str, max_chars: usize) -> String {
        let max_chars = max_chars.max(1);
        let mut cut_byte = input.len();
        for (seen, (idx, _)) in input.char_indices().enumerate() {
            if seen == max_chars {
                cut_byte = idx;
                break;
            }
        }
        input[..cut_byte].to_string()
    }

    #[derive(Debug)]
    struct WindowLine {
        line_no: usize,
        rendered: String,
        rendered_chars: usize,
    }

    let Ok(file) = std::fs::File::open(file_path) else {
        return true;
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut line_no = 0usize;
    let mut range_idx = 0usize;

    while range_idx < ranges.len() {
        let range = &ranges[range_idx];
        let range_start_line = range.start_line.max(opts.file_resume_line);
        if range_start_line > range.end_line {
            range_idx += 1;
            continue;
        }

        if acc.hunks.len() >= opts.max_hunks {
            acc.truncated = true;
            acc.truncation = Some(GrepContextTruncation::MaxHunks);
            acc.next_cursor_state = Some((display_file, range_start_line));
            return false;
        }

        let used_chars_before_range = acc.used_chars;
        let remaining_budget = opts.max_chars.saturating_sub(used_chars_before_range);

        let mut window: VecDeque<WindowLine> = VecDeque::new();
        let mut window_chars = 0usize;
        let mut match_lines = Vec::new();
        let mut anchor_line: Option<usize> = None;
        let mut end_line = range_start_line.saturating_sub(1);
        let mut stop_due_to_budget = false;
        let mut match_idx = 0usize;
        while match_idx < range.match_lines.len() && range.match_lines[match_idx] < range_start_line
        {
            match_idx += 1;
        }

        let pop_front_line = |window: &mut VecDeque<WindowLine>, window_chars: &mut usize| {
            let Some(removed) = window.pop_front() else {
                return;
            };
            if window.is_empty() {
                *window_chars = window_chars.saturating_sub(removed.rendered_chars);
            } else {
                *window_chars =
                    window_chars.saturating_sub(removed.rendered_chars.saturating_add(1));
            }
        };

        let push_line = |window: &mut VecDeque<WindowLine>,
                         window_chars: &mut usize,
                         line: WindowLine| {
            if window.is_empty() {
                *window_chars = window_chars.saturating_add(line.rendered_chars);
            } else {
                *window_chars = window_chars.saturating_add(line.rendered_chars.saturating_add(1));
            }
            window.push_back(line);
        };

        loop {
            line.clear();
            let Ok(bytes_read) = reader.read_line(&mut line) else {
                break;
            };
            if bytes_read == 0 {
                break;
            }
            line_no += 1;

            if line_no < range_start_line {
                continue;
            }
            if line_no > range.end_line {
                break;
            }

            let is_match =
                match_idx < range.match_lines.len() && range.match_lines[match_idx] == line_no;
            if is_match {
                match_idx = match_idx.saturating_add(1);
            }

            let mut rendered = match opts.format {
                ContentFormat::Plain => String::new(),
                ContentFormat::Numbered => {
                    if is_match {
                        format!("{line_no}:* ")
                    } else {
                        format!("{line_no}:  ")
                    }
                }
            };

            let text = line.trim_end_matches(&['\r', '\n'][..]);
            rendered.push_str(text);
            let line_chars = rendered.chars().count();

            if remaining_budget == 0 {
                acc.truncated = true;
                acc.truncation = Some(GrepContextTruncation::MaxChars);
                stop_due_to_budget = true;
                break;
            }
            if line_chars > remaining_budget && window.is_empty() {
                if is_match {
                    let truncated = truncate_to_chars(&rendered, remaining_budget);
                    let truncated_chars = truncated.chars().count();
                    push_line(
                        &mut window,
                        &mut window_chars,
                        WindowLine {
                            line_no,
                            rendered: truncated,
                            rendered_chars: truncated_chars,
                        },
                    );
                    match_lines.push(line_no);
                    end_line = line_no;
                    acc.truncated = true;
                    acc.truncation = Some(GrepContextTruncation::MaxChars);
                    stop_due_to_budget = true;
                    break;
                }

                // Tight-loop UX: avoid returning only prelude context lines. If a non-match line
                // does not fit the remaining budget, skip it and keep scanning until we reach a
                // match line we can include (possibly truncated).
                continue;
            }

            // Under tight budgets we must preserve at least one match line in the returned hunk.
            //
            // Pre-anchor: keep a sliding window (drop earliest lines as needed) so we can always
            // reach the first match line, instead of returning only "prelude" context.
            //
            // Post-anchor: prefer to keep the match line; drop only pre-match lines to make room
            // for post-match context, and stop if we'd have to drop the match itself.
            let mut extra_chars = if window.is_empty() {
                line_chars
            } else {
                1 + line_chars
            };
            if anchor_line.is_none() {
                while window_chars.saturating_add(extra_chars) > remaining_budget
                    && !window.is_empty()
                {
                    pop_front_line(&mut window, &mut window_chars);
                    extra_chars = if window.is_empty() {
                        line_chars
                    } else {
                        1 + line_chars
                    };
                }

                if window_chars.saturating_add(extra_chars) > remaining_budget {
                    if is_match {
                        // If we cannot fit the match line even after dropping all pre-match
                        // context, include a truncated match line (so we still return something
                        // actionable).
                        window.clear();
                        window_chars = 0;
                        let truncated = truncate_to_chars(&rendered, remaining_budget);
                        let truncated_chars = truncated.chars().count();
                        push_line(
                            &mut window,
                            &mut window_chars,
                            WindowLine {
                                line_no,
                                rendered: truncated,
                                rendered_chars: truncated_chars,
                            },
                        );
                        match_lines.push(line_no);
                        end_line = line_no;
                        acc.truncated = true;
                        acc.truncation = Some(GrepContextTruncation::MaxChars);
                        stop_due_to_budget = true;
                        break;
                    }

                    // Can't fit this pre-match line. Drop any collected prelude and keep scanning
                    // for the match line instead of returning noise-only context.
                    window.clear();
                    window_chars = 0;
                    continue;
                }
            } else {
                while window
                    .front()
                    .is_some_and(|front| front.line_no < anchor_line.unwrap_or(usize::MAX))
                    && window_chars.saturating_add(extra_chars) > remaining_budget
                {
                    pop_front_line(&mut window, &mut window_chars);
                    extra_chars = if window.is_empty() {
                        line_chars
                    } else {
                        1 + line_chars
                    };
                }

                if window_chars.saturating_add(extra_chars) > remaining_budget {
                    acc.truncated = true;
                    acc.truncation = Some(GrepContextTruncation::MaxChars);
                    stop_due_to_budget = true;
                    break;
                }
            }

            push_line(
                &mut window,
                &mut window_chars,
                WindowLine {
                    line_no,
                    rendered,
                    rendered_chars: line_chars,
                },
            );
            if is_match {
                match_lines.push(line_no);
                if anchor_line.is_none() {
                    anchor_line = Some(line_no);
                }
            }
            end_line = line_no;
        }

        if stop_due_to_budget && window.is_empty() {
            // Cursor-first contract: even if we cannot fit any lines for this range due to an
            // exhausted content budget, provide a resume point so pagination can continue.
            acc.next_cursor_state = Some((display_file, range_start_line));
            return false;
        }

        let start_line = window
            .front()
            .map(|l| l.line_no)
            .unwrap_or(range_start_line);
        let content = window
            .iter()
            .map(|line| line.rendered.as_str())
            .collect::<Vec<&str>>()
            .join("\n");
        acc.used_chars = used_chars_before_range.saturating_add(window_chars);

        acc.returned_matches += match_lines.len();
        let match_lines = if match_lines.is_empty() {
            None
        } else {
            Some(match_lines)
        };

        acc.hunks.push(GrepContextHunk {
            file: display_file.clone(),
            start_line,
            end_line,
            match_lines,
            content,
        });

        if stop_due_to_budget {
            acc.next_cursor_state = Some((display_file, end_line.saturating_add(1)));
            return false;
        }

        range_idx += 1;
    }

    true
}

fn build_next_cursor(
    root_display: &str,
    request: &GrepContextRequest,
    opts: GrepCursorOptions<'_>,
    cursor_state: Option<(String, usize)>,
) -> Result<Option<String>> {
    let Some((resume_file, resume_line)) = cursor_state else {
        return Ok(None);
    };

    let pattern = request
        .pattern
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let token = GrepContextCursorV1 {
        v: CURSOR_VERSION,
        tool: "rg".to_string(),
        root: Some(root_display.to_string()),
        root_hash: Some(cursor_fingerprint(root_display)),
        pattern: pattern.to_string(),
        file: request
            .file
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        file_pattern: opts.file_pattern.map(str::to_string),
        literal: request.literal.unwrap_or(false),
        case_sensitive: opts.case_sensitive,
        before: opts.before,
        after: opts.after,
        max_matches: opts.max_matches,
        max_hunks: opts.max_hunks,
        max_chars: opts.max_chars,
        format: request.format.unwrap_or(ContentFormat::Numbered),
        allow_secrets: request.allow_secrets.unwrap_or(false),
        resume_file,
        resume_line,
    };

    Ok(Some(encode_cursor(&token)?))
}

pub(super) async fn compute_grep_context_result(
    root: &Path,
    root_display: &str,
    request: &GrepContextRequest,
    regex: &Regex,
    opts: GrepContextComputeOptions<'_>,
) -> Result<GrepContextResult> {
    const MAX_FILE_BYTES: u64 = 2_000_000;
    let GrepContextComputeOptions {
        case_sensitive,
        before,
        after,
        max_matches,
        max_hunks,
        max_chars,
        content_max_chars,
        resume_file,
        resume_line,
    } = opts;

    let file_pattern = trimmed_non_empty_str(request.file_pattern.as_deref());
    let pattern = request
        .pattern
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Pattern must not be empty"))?
        .to_string();
    let format = request.format.unwrap_or(ContentFormat::Numbered);
    let resume_file = trimmed_non_empty_str(resume_file);
    let resume_line = resume_line.max(1);
    let (source, candidates) = collect_candidates(root, request, file_pattern).await?;
    ensure_resume_file_exists(resume_file, &candidates)?;

    let mut acc = GrepContextAccumulators::new();
    let mut started = resume_file.is_none();
    'outer_files: for (display_file, file_path) in candidates {
        if !started {
            if Some(display_file.as_str()) != resume_file {
                continue;
            }
            started = true;
        }

        let file_resume_line = file_resume_line(display_file.as_str(), resume_file, resume_line);

        acc.scanned_files += 1;

        let Ok(meta) = std::fs::metadata(&file_path) else {
            continue;
        };
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }

        let Ok(scan) = scan_match_lines_for_file(
            &file_path,
            regex,
            file_resume_line,
            max_matches,
            &mut acc.total_matches,
        ) else {
            continue;
        };

        if scan.match_lines.is_empty() {
            continue;
        }
        acc.matched_files += 1;
        if scan.hit_match_limit {
            acc.truncated = true;
            acc.truncation = Some(GrepContextTruncation::MaxMatches);
        }

        let ranges = build_ranges_from_matches(&scan.match_lines, before, after);

        if !build_hunks_for_file(
            &mut acc,
            display_file,
            &file_path,
            &ranges,
            GrepHunkBuildOptions {
                file_resume_line,
                max_hunks,
                max_chars: content_max_chars,
                format,
            },
        ) {
            break 'outer_files;
        }

        if scan.hit_match_limit {
            break 'outer_files;
        }
    }

    let next_cursor = build_next_cursor(
        root_display,
        request,
        GrepCursorOptions {
            file_pattern,
            case_sensitive,
            before,
            after,
            max_matches,
            max_hunks,
            max_chars,
        },
        acc.next_cursor_state.take(),
    )?;

    let result = GrepContextResult {
        pattern,
        source: Some(source),
        file: request.file.clone(),
        file_pattern: request.file_pattern.clone(),
        case_sensitive: Some(case_sensitive),
        before: Some(before),
        after: Some(after),
        scanned_files: Some(acc.scanned_files),
        matched_files: Some(acc.matched_files),
        returned_matches: Some(acc.returned_matches),
        returned_hunks: Some(acc.hunks.len()),
        used_chars: Some(acc.used_chars),
        max_chars: Some(max_chars),
        truncated: acc.truncated,
        truncation: acc.truncation,
        next_cursor,
        next_actions: None,
        meta: Some(ToolMeta::default()),
        hunks: acc.hunks,
    };

    Ok(result)
}
