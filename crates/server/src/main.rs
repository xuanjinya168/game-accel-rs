use anyhow::Result;
use clap::Parser;
use server::{ServerOptions, run_server};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "game-accel-server",
    about = "All-in-one QUIC/UDP game accelerator & built-in Web Admin Panel"
)]
struct Args {
    /// QUIC UDP listen address
    #[arg(short, long, default_value = "0.0.0.0:8443")]
    listen: SocketAddr,

    /// Built-in Web Admin Panel TCP listen address (default loopback-only; use "none" to disable)
    #[arg(long, default_value = "127.0.0.1:9090")]
    panel_listen: String,

    /// Persistent panel & multi-link state JSON file
    #[arg(long, default_value = "/etc/game-accel/data.json")]
    data_file: PathBuf,

    /// Initial link token used when data-file does not yet exist
    #[arg(short, long, default_value = "dev-secret-token")]
    token: String,

    /// Optional initial admin password override for Web Panel
    #[arg(long)]
    panel_pass: Option<String>,

    /// Public IP or domain embedded into generated gacl:// links
    #[arg(long, default_value = "127.0.0.1")]
    public_host: String,

    /// TLS SNI domain embedded into generated gacl:// links
    #[arg(long, default_value = "localhost")]
    sni: String,

    /// Path to PEM certificate chain (fullchain.pem)
    #[arg(long)]
    cert: Option<PathBuf>,

    /// Path to PEM private key (privkey.pem)
    #[arg(long)]
    key: Option<PathBuf>,

    /// Self-signed cert SAN domain when --cert/--key are omitted
    #[arg(long, default_value = "localhost")]
    dev_domain: String,

    /// Optional static web root directory served over HTTP/3 to unauthenticated probes/browsers
    #[arg(long)]
    masquerade_dir: Option<PathBuf>,

    /// Optional Brutal congestion-control target rate in Mbps for server listener QUIC transport (0 = BBR default)
    #[arg(long, default_value_t = 0)]
    brutal_mbps: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let _ = rustls::crypto::ring::default_provider().install_default();
    let args = Args::parse();

    run_server(ServerOptions {
        listen: args.listen,
        panel_listen: args.panel_listen,
        data_file: args.data_file,
        token: args.token,
        panel_pass: args.panel_pass,
        public_host: args.public_host,
        sni: args.sni,
        cert: args.cert,
        key: args.key,
        dev_domain: args.dev_domain,
        masquerade_dir: args.masquerade_dir,
        brutal_mbps: args.brutal_mbps,
    })
    .await
}
