#[tokio::main]
async fn main() -> anyhow::Result<()> {
    agentlog::cli::run().await
}
