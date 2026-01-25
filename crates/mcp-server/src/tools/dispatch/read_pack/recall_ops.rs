use super::recall::OpsIntent;

pub(super) fn ops_intent(question: &str) -> Option<OpsIntent> {
    let q = question.to_lowercase();

    let contains_ascii_token = |needle: &str| {
        q.split(|c: char| !c.is_ascii_alphanumeric())
            .any(|tok| tok == needle)
    };

    // Highly specific ops: visual regression / golden snapshot workflows.
    //
    // Keep it strict: require snapshot/golden keywords (GPU alone should not redirect from "run").
    let mentions_snapshots = [
        "snapshot",
        "snapshots",
        "golden",
        "goldens",
        "baseline",
        "screenshot",
        "visual regression",
        "update_snapshots",
        "update-snapshots",
        "update_snapshot",
        "update-snapshot",
        "снапшот",
        "скриншот",
        "голден",
    ]
    .iter()
    .any(|needle| q.contains(needle));
    if mentions_snapshots {
        return Some(OpsIntent::Snapshots);
    }

    let mentions_quality = [
        "quality gate",
        "quality gates",
        "quality-gate",
        "quality_gates",
        "quality",
        "гейт",
        "гейты",
        "проверки",
        "линт",
        "lint",
        "clippy",
        "fmt",
        "format",
        "validate_contracts",
    ]
    .iter()
    .any(|needle| q.contains(needle));

    let mentions_test = [
        "test",
        "tests",
        "testing",
        "pytest",
        "cargo test",
        "go test",
        "npm test",
        "yarn test",
        "pnpm test",
        "тест",
    ]
    .iter()
    .any(|needle| q.contains(needle));

    // Avoid substring false-positives ("velocity" contains "ci"). Prefer token detection for CI.
    if mentions_quality || mentions_test || contains_ascii_token("ci") || q.contains("pipeline") {
        return Some(OpsIntent::TestAndGates);
    }

    if [
        "run",
        "start",
        "serve",
        "dev",
        "local",
        "launch",
        "запуск",
        "запустить",
        "старт",
        "локально",
    ]
    .iter()
    .any(|needle| q.contains(needle))
    {
        return Some(OpsIntent::Run);
    }

    if ["build", "compile", "собрат", "сборк"]
        .iter()
        .any(|needle| q.contains(needle))
    {
        return Some(OpsIntent::Build);
    }

    if [
        "deploy",
        "release",
        "prod",
        "production",
        "депло",
        "разверн",
        "релиз",
    ]
    .iter()
    .any(|needle| q.contains(needle))
    {
        return Some(OpsIntent::Deploy);
    }

    if [
        "install",
        "setup",
        "configure",
        "init",
        "установ",
        "настро",
        "конфиг",
    ]
    .iter()
    .any(|needle| q.contains(needle))
    {
        return Some(OpsIntent::Setup);
    }

    None
}

pub(super) fn ops_grep_pattern(intent: OpsIntent) -> &'static str {
    match intent {
        OpsIntent::TestAndGates => {
            // Prefer concrete commands / recipes across ecosystems.
            r"(?m)(^\s*(test|tests|check|gate|lint|fmt|format)\s*:|scripts/validate_contracts\.sh|validate_contracts|cargo\s+fmt\b|fmt\b.*--check|cargo\s+clippy\b|clippy\b.*--workspace|cargo\s+xtask\s+(check|gate)\b|cargo\s+test\b|CONTEXT_EMBEDDING_MODE=stub\s+cargo\s+test\b|cargo\s+nextest\b|pytest\b|go\s+test\b|npm\s+test\b|yarn\s+test\b|pnpm\s+test\b|just\s+(test|check|gate|lint|fmt)\b|make\s+test\b|make\s+check\b)"
        }
        OpsIntent::Snapshots => {
            // Visual regression / golden snapshot workflows across ecosystems.
            // Prefer actionable "update baseline" commands and env knobs.
            r"(?mi)(snapshot|snapshots|golden|goldens|baseline|screenshot|visual\s+regression|update[_-]?snapshots|--update[-_]?snapshots|update[_-]?snapshot|--update[-_]?snapshot|update[_-]?baseline|--update[-_]?baseline|record[_-]?snapshots|APEX_UPDATE_SNAPSHOTS|UPDATE_SNAPSHOTS|SNAPSHOT|GOLDEN|baseline\s+image)"
        }
        OpsIntent::Run => {
            r"(?m)(^\s*(run|start|dev|serve)\s*:|cargo\s+run\b|python\s+-m\b|uv\s+run\b|poetry\s+run\b|npm\s+run\s+dev\b|npm\s+start\b|yarn\s+dev\b|pnpm\s+dev\b|just\s+(run|start|dev)\b|make\s+run\b|docker\s+compose\s+up\b)"
        }
        OpsIntent::Build => {
            r"(?m)(^\s*(build|compile)\s*:|cargo\s+build\b|go\s+build\b|python\s+-m\s+build\b|npm\s+run\s+build\b|yarn\s+build\b|pnpm\s+build\b|just\s+build\b|make\s+build\b|cmake\b|bazel\b)"
        }
        OpsIntent::Deploy => {
            r"(?m)(^\s*(deploy|release|prod)\s*:|deploy\b|release\b|docker\s+build\b|docker\s+compose\b|kubectl\b|helm\b|terraform\b)"
        }
        OpsIntent::Setup => {
            r"(?m)(^\s*(install|setup|init|configure)\s*:|pip\s+install\b|poetry\s+install\b|uv\s+sync\b|npm\s+install\b|pnpm\s+install\b|yarn\b\s+install\b|cargo\s+install\b|just\s+install\b|make\s+install\b)"
        }
    }
}
