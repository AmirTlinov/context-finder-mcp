use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    context_mcp::main_entry().await
}
