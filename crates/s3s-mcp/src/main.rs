use s3s_fs::FileSystem;
use s3s_mcp::McpServer;

use std::io::IsTerminal;
use std::path::PathBuf;

use clap::Parser;
use rmcp::ServiceExt;
use tracing::info;

#[derive(Debug, Parser)]
#[command(version)]
struct Opt {
    /// Root directory of stored data.
    root: PathBuf,
}

fn setup_tracing() {
    use tracing_subscriber::EnvFilter;

    let env_filter = EnvFilter::from_default_env();
    let enable_color = std::io::stderr().is_terminal();

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_ansi(enable_color)
        .with_writer(std::io::stderr)
        .init();
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let opt = Opt::parse();
    setup_tracing();
    run(opt)
}

#[tokio::main]
async fn run(opt: Opt) -> Result<(), Box<dyn std::error::Error>> {
    let fs = FileSystem::new(opt.root).map_err(|e| format!("{e:?}"))?;
    let server = McpServer::new(fs);

    info!("starting s3s-mcp server on stdio");
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
