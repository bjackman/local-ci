#[cfg(test)]
mod test_utils;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    local_ci::main().await
}