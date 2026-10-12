//! Alosa control server.
//!
//! Devices connect over `/device` (Noise-encrypted protobuf), the Home
//! Assistant integration connects over `/api` (JSON, bearer token). Runs as a
//! Home Assistant add-on, or standalone.

mod api_ws;
mod device_ws;
mod mdns;
mod registry;
mod views;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use alosa_protocol::{
    noise::{self, Identity},
    DEFAULT_PORT, DEVICE_WS_PATH,
};
use anyhow::Context;
use axum::{
    extract::{ConnectInfo, State, WebSocketUpgrade},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use clap::{ArgAction, Parser};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::registry::Registry;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Port for both the device and API endpoints.
    #[arg(long, env = "ALOSA_PORT", default_value_t = DEFAULT_PORT)]
    port: u16,

    /// Where the device store, server key and API token are kept. The add-on
    /// sets this to /data.
    #[arg(long, env = "ALOSA_DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,

    /// Adopt new devices automatically instead of waiting for approval in
    /// Home Assistant. Convenient for development, not for real homes.
    #[arg(long, env = "ALOSA_AUTO_ADOPT", default_value_t = false, action = ArgAction::Set)]
    auto_adopt: bool,

    /// Advertise the server via mDNS so devices find it automatically.
    #[arg(long, env = "ALOSA_MDNS", default_value_t = true, action = ArgAction::Set)]
    mdns: bool,
}

#[derive(Clone)]
struct AppState {
    registry: Arc<Registry>,
    identity: Arc<Identity>,
    server_id: Arc<str>,
    api_token: Arc<str>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args = Args::parse();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        ?args,
        "starting Alosa server"
    );

    let registry = Registry::load(args.data_dir.clone(), args.auto_adopt).await?;

    let key_path = args.data_dir.join("server.key");
    let identity = Identity::load_or_create(&key_path)
        .with_context(|| format!("loading server key {}", key_path.display()))?;
    let server_id: Arc<str> = noise::fingerprint(identity.public_key()).into();
    info!(%server_id, "server identity loaded");

    let api_token = load_or_create_api_token(&args.data_dir)?;

    let _mdns = if args.mdns {
        match mdns::advertise(args.port) {
            Ok(daemon) => Some(daemon),
            Err(e) => {
                warn!("mDNS advertising failed, devices will need the server address configured: {e:#}");
                None
            }
        }
    } else {
        None
    };

    let state = AppState {
        registry,
        identity: Arc::new(identity),
        server_id,
        api_token,
    };
    let app = Router::new()
        .route(DEVICE_WS_PATH, get(device_ws))
        .route("/api", get(api_ws))
        .route("/health", get(|| async { "ok" }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", args.port)).await?;
    info!(addr = %listener.local_addr()?, "listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        shutdown_signal().await;
        info!("shutting down");
    })
    .await?;
    Ok(())
}

/// Resolves on Ctrl-C or SIGTERM (what the Supervisor and Docker send to
/// stop a container).
async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

/// The token the Home Assistant integration must present. Generated on first
/// run; the add-on passes it to Home Assistant through Supervisor discovery.
fn load_or_create_api_token(data_dir: &std::path::Path) -> anyhow::Result<Arc<str>> {
    let path = data_dir.join("api_token");
    match std::fs::read_to_string(&path) {
        Ok(token) if !token.trim().is_empty() => return Ok(token.trim().into()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("generating API token: {e}"))?;
    let token = noise::to_hex(&bytes);
    noise::write_private(&path, token.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    info!(path = %path.display(), "generated a new API token; Home Assistant needs it to connect");
    Ok(token.into())
}

/// Compares in constant time, so response timing doesn't leak the token.
fn token_matches(given: &str, expected: &str) -> bool {
    given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

async fn device_ws(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| device_ws::handle(socket, addr, state.registry, state.identity))
}

async fn api_ws(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if !token.is_some_and(|t| token_matches(t, &state.api_token)) {
        warn!(%addr, "rejected API connection with a missing or wrong token");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |socket| api_ws::handle(socket, state.registry, state.server_id))
}
