use crate::command::{EvalCompareOutput, EvalOutput};
use anyhow::Result;
use std::path::Path;
use std::process::Command;

pub fn render_eval_report(project_root: &Path, out: &EvalOutput) -> Result<String> {
    let git = git_head(project_root);

    let mut md = String::new();
    md.push_str("# Context eval report\n\n");
    md.push_str(&format!("- Project: `{}`\n", project_root.display()));
    md.push_str(&format!("- Git: `{}`\n", git.as_deref().unwrap_or("n/a")));
    md.push_str(&format!(
        "- Dataset: `{}` (schema_version={})\n",
        out.dataset.name.as_deref().unwrap_or("unnamed"),
        out.dataset.schema_version
    ));
    md.push_str(&format!("- Cases: `{}`\n\n", out.dataset.cases));

    md.push_str("## Runs\n\n");
    md.push_str("| profile | cache | models | mean_mrr | mean_recall | mean_overlap | p95_ms | mean_bytes |\n");
    md.push_str("|---|---|---:|---:|---:|---:|---:|---:|\n");
    for run in &out.runs {
        md.push_str(&format!(
            "| `{}` | `{}` | `{}` | `{:.3}` | `{:.3}` | `{:.3}` | `{}` | `{:.1}` |\n",
            run.profile,
            format!("{:?}", run.cache_mode).to_lowercase(),
            run.models.len(),
            run.summary.mean_mrr,
            run.summary.mean_recall,
            run.summary.mean_overlap_ratio,
            run.summary.p95_latency_ms,
            run.summary.mean_bytes
        ));
    }
    md.push('\n');

    for run in &out.runs {
        let mut cases: Vec<_> = run.cases.iter().collect();
        cases.sort_by(|a, b| {
            a.mrr
                .total_cmp(&b.mrr)
                .then_with(|| a.recall.total_cmp(&b.recall))
                .then_with(|| a.id.cmp(&b.id))
        });
        md.push_str(&format!("## Worst cases (profile `{}`)\n\n", run.profile));
        md.push_str("| id | mrr | recall | first_rank | query |\n");
        md.push_str("|---|---:|---:|---:|---|\n");
        for case in cases.into_iter().take(10) {
            md.push_str(&format!(
                "| `{}` | `{:.3}` | `{:.3}` | `{}` | `{}` |\n",
                case.id,
                case.mrr,
                case.recall,
                case.first_rank.map_or("n/a".to_string(), |v| v.to_string()),
                escape_cell(&truncate_one_line(&case.query, 120)),
            ));
        }
        md.push('\n');
    }

    Ok(md)
}

pub fn render_eval_compare_report(project_root: &Path, out: &EvalCompareOutput) -> Result<String> {
    let git = git_head(project_root);

    let mut md = String::new();
    md.push_str("# Context eval compare report\n\n");
    md.push_str(&format!("- Project: `{}`\n", project_root.display()));
    md.push_str(&format!("- Git: `{}`\n", git.as_deref().unwrap_or("n/a")));
    md.push_str(&format!(
        "- Dataset: `{}` (schema_version={})\n",
        out.dataset.name.as_deref().unwrap_or("unnamed"),
        out.dataset.schema_version
    ));
    md.push_str(&format!("- Cases: `{}`\n", out.dataset.cases));
    md.push_str(&format!(
        "- Cache mode: `{}`\n\n",
        format!("{:?}", out.cache_mode).to_lowercase()
    ));

    md.push_str("## Summary (B - A)\n\n");
    md.push_str(&format!(
        "- A: profile `{}`, models `{}`\n",
        out.a.profile,
        out.a.models.join(", ")
    ));
    md.push_str(&format!(
        "- B: profile `{}`, models `{}`\n",
        out.b.profile,
        out.b.models.join(", ")
    ));
    md.push_str(&format!(
        "- Δ mean_mrr: `{:.3}`, Δ mean_recall: `{:.3}`, Δ p95_ms: `{}`\n",
        out.summary.delta_mean_mrr, out.summary.delta_mean_recall, out.summary.delta_p95_latency_ms
    ));
    md.push_str(&format!(
        "- Wins: A `{}`, B `{}`, ties `{}`\n\n",
        out.summary.a_wins, out.summary.b_wins, out.summary.ties
    ));

    let mut regressions: Vec<_> = out.cases.iter().filter(|c| c.delta_mrr < 0.0).collect();
    regressions.sort_by(|a, b| {
        a.delta_mrr
            .total_cmp(&b.delta_mrr)
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut improvements: Vec<_> = out.cases.iter().filter(|c| c.delta_mrr > 0.0).collect();
    improvements.sort_by(|a, b| {
        b.delta_mrr
            .total_cmp(&a.delta_mrr)
            .then_with(|| a.id.cmp(&b.id))
    });

    md.push_str("## Top regressions (Δmrr < 0)\n\n");
    md.push_str("| id | Δmrr | Δrecall | Δlatency_ms | query |\n");
    md.push_str("|---|---:|---:|---:|---|\n");
    for case in regressions.into_iter().take(10) {
        md.push_str(&format!(
            "| `{}` | `{:.3}` | `{:.3}` | `{}` | `{}` |\n",
            case.id,
            case.delta_mrr,
            case.delta_recall,
            case.delta_latency_ms,
            escape_cell(&truncate_one_line(&case.query, 120)),
        ));
    }
    md.push('\n');

    md.push_str("## Top improvements (Δmrr > 0)\n\n");
    md.push_str("| id | Δmrr | Δrecall | Δlatency_ms | query |\n");
    md.push_str("|---|---:|---:|---:|---|\n");
    for case in improvements.into_iter().take(10) {
        md.push_str(&format!(
            "| `{}` | `{:.3}` | `{:.3}` | `{}` | `{}` |\n",
            case.id,
            case.delta_mrr,
            case.delta_recall,
            case.delta_latency_ms,
            escape_cell(&truncate_one_line(&case.query, 120)),
        ));
    }
    md.push('\n');

    Ok(md)
}

fn git_head(project_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn truncate_one_line(text: &str, max_chars: usize) -> String {
    let mut s = text.replace(['\n', '\r', '\t'], " ");
    s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max_chars {
        return s;
    }
    let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{truncated}…")
}

fn escape_cell(text: &str) -> String {
    text.replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{
        EvalCacheMode, EvalCaseResult, EvalCompareCase, EvalCompareOutput, EvalCompareSummary,
        EvalDatasetMeta, EvalHit, EvalOutput, EvalRun, EvalRunSummary, EvalSummary,
    };

    #[test]
    fn eval_report_renders_headers() {
        let out = EvalOutput {
            dataset: EvalDatasetMeta {
                schema_version: 1,
                name: Some("smoke".to_string()),
                cases: 1,
            },
            runs: vec![EvalRun {
                profile: "general".to_string(),
                models: vec!["bge-small".to_string()],
                limit: 5,
                cache_mode: EvalCacheMode::Warm,
                summary: EvalSummary {
                    mean_mrr: 1.0,
                    mean_recall: 1.0,
                    mean_overlap_ratio: 1.0,
                    mean_latency_ms: 5.0,
                    p50_latency_ms: 5,
                    p95_latency_ms: 6,
                    mean_bytes: 123.0,
                    anchor_cases: 0,
                    anchor_hit_cases: 0,
                    anchorless_cases: 0,
                    anchorless_rate: 0.0,
                },
                cases: vec![EvalCaseResult {
                    id: "case1".to_string(),
                    query: "q".to_string(),
                    expected_paths: vec!["src/lib.rs".to_string()],
                    expected_symbols: Vec::new(),
                    intent: None,
                    mrr: 1.0,
                    recall: 1.0,
                    overlap_ratio: 1.0,
                    first_rank: Some(1),
                    latency_ms: 5,
                    bytes: 100,
                    hits: vec![EvalHit {
                        id: "src/lib.rs:1:2".to_string(),
                        file: "src/lib.rs".to_string(),
                        start_line: 1,
                        end_line: 2,
                        score: 1.0,
                    }],
                }],
            }],
        };

        let md = render_eval_report(Path::new("/tmp"), &out).expect("report");
        assert!(md.contains("# Context eval report"));
        assert!(md.contains("## Runs"));
        assert!(md.contains("Worst cases"));
    }

    #[test]
    fn eval_compare_report_renders_headers() {
        let out = EvalCompareOutput {
            dataset: EvalDatasetMeta {
                schema_version: 1,
                name: Some("smoke".to_string()),
                cases: 1,
            },
            cache_mode: EvalCacheMode::Warm,
            a: EvalRunSummary {
                profile: "a".to_string(),
                models: vec!["bge-small".to_string()],
                limit: 5,
                cache_mode: EvalCacheMode::Warm,
                summary: EvalSummary {
                    mean_mrr: 0.5,
                    mean_recall: 1.0,
                    mean_overlap_ratio: 0.5,
                    mean_latency_ms: 10.0,
                    p50_latency_ms: 10,
                    p95_latency_ms: 11,
                    mean_bytes: 100.0,
                    anchor_cases: 0,
                    anchor_hit_cases: 0,
                    anchorless_cases: 0,
                    anchorless_rate: 0.0,
                },
            },
            b: EvalRunSummary {
                profile: "b".to_string(),
                models: vec!["bge-small".to_string()],
                limit: 5,
                cache_mode: EvalCacheMode::Warm,
                summary: EvalSummary {
                    mean_mrr: 1.0,
                    mean_recall: 1.0,
                    mean_overlap_ratio: 1.0,
                    mean_latency_ms: 9.0,
                    p50_latency_ms: 9,
                    p95_latency_ms: 9,
                    mean_bytes: 110.0,
                    anchor_cases: 0,
                    anchor_hit_cases: 0,
                    anchorless_cases: 0,
                    anchorless_rate: 0.0,
                },
            },
            summary: EvalCompareSummary {
                delta_mean_mrr: 0.5,
                delta_mean_recall: 0.0,
                delta_mean_overlap_ratio: 0.5,
                delta_mean_latency_ms: -1.0,
                delta_p95_latency_ms: -2,
                delta_mean_bytes: 10.0,
                a_wins: 0,
                b_wins: 1,
                ties: 0,
            },
            cases: vec![EvalCompareCase {
                id: "case1".to_string(),
                query: "q".to_string(),
                expected_paths: vec!["src/lib.rs".to_string()],
                a_mrr: 0.5,
                b_mrr: 1.0,
                delta_mrr: 0.5,
                a_recall: 1.0,
                b_recall: 1.0,
                delta_recall: 0.0,
                a_overlap_ratio: 0.5,
                b_overlap_ratio: 1.0,
                delta_overlap_ratio: 0.5,
                a_latency_ms: 10,
                b_latency_ms: 9,
                delta_latency_ms: -1,
                a_bytes: 100,
                b_bytes: 110,
                delta_bytes: 10,
                a_first_rank: Some(2),
                b_first_rank: Some(1),
            }],
        };

        let md = render_eval_compare_report(Path::new("/tmp"), &out).expect("report");
        assert!(md.contains("# Context eval compare report"));
        assert!(md.contains("Summary (B - A)"));
        assert!(md.contains("Top regressions"));
        assert!(md.contains("Top improvements"));
    }
}
