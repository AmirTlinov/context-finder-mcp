pub(super) fn contract_candidate_score(rel: &str) -> i32 {
    let normalized = rel.replace('\\', "/").to_ascii_lowercase();
    match normalized.as_str() {
        "docs/contracts/protocol.md" => 300,
        "docs/contracts/readme.md" => 280,
        "contracts/http/v1/openapi.json" => 260,
        "contracts/http/v1/openapi.yaml" | "contracts/http/v1/openapi.yml" => 255,
        "openapi.json" | "openapi.yaml" | "openapi.yml" => 250,
        "proto/command.proto" => 240,
        "architecture.md" | "docs/architecture.md" => 220,
        "readme.md" => 210,
        _ if normalized.starts_with("docs/contracts/") && normalized.ends_with(".md") => 200,
        _ if normalized.starts_with("contracts/") => 180,
        _ if normalized.starts_with("proto/") && normalized.ends_with(".proto") => 170,
        _ => 10,
    }
}
