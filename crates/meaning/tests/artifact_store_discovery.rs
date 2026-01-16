use anyhow::Result;
use context_meaning::{meaning_pack, MeaningPackRequest};

#[tokio::test]
async fn meaning_pack_discovers_artifact_store_file_under_ignored_data_scope() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();

    std::fs::write(root.join("README.md"), "# Demo\n")?;
    std::fs::create_dir_all(root.join("data"))?;
    std::fs::write(root.join("data").join("README.md"), "# Data\n")?;

    let request = MeaningPackRequest {
        query: "onboarding".to_string(),
        map_depth: None,
        map_limit: None,
        max_chars: Some(4_000),
    };
    let result = meaning_pack(root, &root.to_string_lossy(), &request).await?;

    assert!(
        result.pack.contains("ANCHOR kind=artifact"),
        "expected artifact anchor when data/README.md exists (pack={})",
        result.pack
    );

    Ok(())
}
