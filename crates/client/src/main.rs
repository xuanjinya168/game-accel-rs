use anyhow::Result;
use clap::Parser;
use client::rules::{DomesticEgress, RouteMode, RouteModeCell};
use client::socks5::ClientStats;
use client::{ClientOptions, UpstreamOptions, run_client_with_shutdown};
use protocol::share::NodeProtocol;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Parser, Debug)]
#[command(
    name = "game-accel-client",
    about = "Minimal QUIC/UDP game accelerator & proxy client"
)]
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

    /// Transport protocol: gacl or hysteria2 (hy2)
    #[arg(long, default_value = "gacl", value_parser = parse_protocol)]
    protocol: NodeProtocol,

    #[arg(long, default_value_t = true)]
    game_dup: bool,

    #[arg(long, default_value_t = false)]
    tun: bool,

    #[arg(long, default_value_t = false)]
    insecure: bool,

    /// Optional Brutal congestion-control target rate in Mbps for TCP lane only (0 = BBR default)
    #[arg(long, default_value_t = 0)]
    brutal_mbps: u32,

    /// Routing mode: rule (split CN direct / rest proxied), global, or direct
    #[arg(long, default_value = "rule", value_parser = parse_route_mode)]
    route: RouteMode,

    /// Domestic (CN) egress: direct or node (node requires --secondary)
    #[arg(long, default_value = "direct", value_parser = parse_domestic)]
    domestic: DomesticEgress,

    /// Domestic node address; enables the secondary tunnel
    #[arg(long)]
    secondary: Option<String>,

    /// Domestic node SNI (defaults to --server-name)
    #[arg(long)]
    secondary_server_name: Option<String>,

    /// Domestic node auth token (defaults to --token)
    #[arg(long)]
    secondary_token: Option<String>,

    /// Domestic node protocol (defaults to --protocol)
    #[arg(long, value_parser = parse_protocol)]
    secondary_protocol: Option<NodeProtocol>,

    /// Directory with direct-list.txt / proxy-list.txt / geoip-cn.txt overriding built-in rules
    #[arg(long)]
    rules_dir: Option<std::path::PathBuf>,
}

fn parse_protocol(s: &str) -> Result<NodeProtocol, String> {
    match s.to_ascii_lowercase().as_str() {
        "gacl" => Ok(NodeProtocol::Gacl),
        "hy2" | "hysteria2" => Ok(NodeProtocol::Hysteria2),
        other => Err(format!("unknown protocol '{other}' (gacl|hy2|hysteria2)")),
    }
}

fn parse_route_mode(s: &str) -> Result<RouteMode, String> {
    match s.to_ascii_lowercase().as_str() {
        "rule" => Ok(RouteMode::Rule),
        "dual" => Ok(RouteMode::Dual),
        "global" => Ok(RouteMode::Global),
        "direct" => Ok(RouteMode::Direct),
        other => Err(format!(
            "unknown route mode '{other}' (rule|dual|global|direct)"
        )),
    }
}

fn parse_domestic(s: &str) -> Result<DomesticEgress, String> {
    match s.to_ascii_lowercase().as_str() {
        "direct" => Ok(DomesticEgress::Direct),
        "node" | "secondary" => Ok(DomesticEgress::Secondary),
        other => Err(format!("unknown domestic egress '{other}' (direct|node)")),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let (_tx, rx) = watch::channel(false);
    let stats = Arc::new(ClientStats::default());

    let secondary = args.secondary.map(|server| UpstreamOptions {
        server,
        server_name: args
            .secondary_server_name
            .clone()
            .unwrap_or_else(|| args.server_name.clone()),
        token: args
            .secondary_token
            .clone()
            .unwrap_or_else(|| args.token.clone()),
        protocol: args.secondary_protocol.unwrap_or(args.protocol),
        game_dup: args.game_dup,
        insecure: args.insecure,
        brutal_mbps: 0,
    });
    let domestic = if secondary.is_some()
        && (args.route == RouteMode::Dual || args.domestic == DomesticEgress::Secondary)
    {
        DomesticEgress::Secondary
    } else {
        DomesticEgress::Direct
    };

    run_client_with_shutdown(
        ClientOptions {
            primary: UpstreamOptions {
                server: args.server,
                server_name: args.server_name,
                token: args.token,
                protocol: args.protocol,
                game_dup: args.game_dup,
                insecure: args.insecure,
                brutal_mbps: args.brutal_mbps,
            },
            secondary,
            domestic,
            listen: args.listen,
            enable_tun: args.tun,
            route_mode: RouteModeCell::new(args.route),
            rules_dir: args.rules_dir,
        },
        stats,
        None,
        rx,
    )
    .await
}
