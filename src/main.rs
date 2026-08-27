use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    rib::run().await
}
