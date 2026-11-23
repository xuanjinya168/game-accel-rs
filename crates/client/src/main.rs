use anyhow::Result;
use clap::Parser;
use client::socks5::ClientStats;
use client::{ClientOptions, run_client_with_shutdown};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Parser, Debug)]
#[command(name = "game-accel-client", about = "Minimal QUIC/UDP game accelerator & proxy client")]
struct Args {
    #[arg(short, long, default_value = "127.0.0.1:4433")]
    server: SocketAddr,

    #[arg(long, default_value = "localhost")]
    server_name: String,

    #[arg(short, long, default_value = "127.0.0.1:1080")]
    listen: SocketAddr,

    #[arg(short, long, default_value = "dev-secret-token")]
    token: String,

    #[arg(long, default_value_t = true)]
    game_dup: bool,

    #[arg(long, default_value_t = false)]
    tun: bool,

    #[arg(long, default_value_t = false)]
    insecure: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let (_tx, rx) = watch::channel(false);
    let stats = Arc::new(ClientStats::default());

    run_client_with_shutdown(
        ClientOptions {
            server: args.server,
            server_name: args.server_name,
            listen: args.listen,
            token: args.token,
            game_dup: args.game_dup,
            enable_tun: args.tun,
            insecure: args.insecure,
        },
        stats,
        None,
        rx,
    )
    .await
}
