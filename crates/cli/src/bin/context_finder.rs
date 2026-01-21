use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    context_cli::main_entry().await
}
