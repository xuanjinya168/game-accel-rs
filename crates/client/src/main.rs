use anyhow::Result;
use clap::Parser;
use client::rules::{RouteMode, RouteModeCell};
use client::socks5::ClientStats;
use client::{ClientOptions, run_client_with_shutdown};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Parser, Debug)]
#[command(name = "game-accel-client", about = "Minimal QUIC/UDP game accelerator & proxy client")]
struct Args {
    /// Server address (`IP:port`, `[IPv6]:port`, or `domain:port`)
    #[arg(short, long, default_value = "127.0.0.1:4433")]
    server: String,

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

    /// Routing mode: rule (split CN direct / rest proxied), global, or direct
    #[arg(long, default_value = "rule", value_parser = parse_route_mode)]
    route: RouteMode,

    /// Directory with direct-list.txt / proxy-list.txt / geoip-cn.txt overriding built-in rules
    #[arg(long)]
    rules_dir: Option<std::path::PathBuf>,
}

fn parse_route_mode(s: &str) -> Result<RouteMode, String> {
    match s.to_ascii_lowercase().as_str() {
        "rule" => Ok(RouteMode::Rule),
        "global" => Ok(RouteMode::Global),
        "direct" => Ok(RouteMode::Direct),
        other => Err(format!("unknown route mode '{other}' (rule|global|direct)")),
    }
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
            route_mode: RouteModeCell::new(args.route),
            rules_dir: args.rules_dir,
        },
        stats,
        None,
        rx,
    )
    .await
}
