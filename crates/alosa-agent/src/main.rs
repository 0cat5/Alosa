//! Alosa device agent for Linux-class devices.
//!
//! Finds the server (mDNS or --server), runs the encrypted handshake, says
//! Hello, sends heartbeats, and handles commands. Reconnects forever with
//! backoff, because a device should recover on its own from server restarts
//! and network blips.
//!
//! Pairing: the device trusts whichever server adopts it, and from then on
//! only that server (its key is pinned in the state dir). `--reset` forgets
//! the pinned server, like a factory reset.

mod profiles;

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use alosa_protocol::{
    noise::{self, Handshake, Identity, Receiver, Sender},
    v1, DEVICE_WS_PATH, MDNS_SERVICE_TYPE, PROTOCOL_VERSION,
};
use anyhow::{bail, Context};
use clap::Parser;
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use prost::Message as _;
use tokio::net::TcpStream;
use tokio_tungstenite::{tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::profiles::Profile;

const MAX_BACKOFF: Duration = Duration::from_secs(30);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Server URL, e.g. ws://192.168.1.10:8765/device. Found via mDNS if not set.
    #[arg(long, env = "ALOSA_SERVER")]
    server: Option<String>,

    /// Which kind of device to present as.
    #[arg(long, value_enum, default_value_t = Profile::Connect)]
    profile: Profile,

    /// Unique device ID. Derived from /etc/machine-id if not set.
    #[arg(long, env = "ALOSA_DEVICE_ID")]
    device_id: Option<String>,

    /// Suggested friendly name.
    #[arg(long, env = "ALOSA_DEVICE_NAME")]
    name: Option<String>,

    /// Where the device key and pinned server key are kept. Defaults to
    /// ~/.local/state/alosa-agent/<device id>.
    #[arg(long, env = "ALOSA_STATE_DIR")]
    state_dir: Option<PathBuf>,

    /// Forget the paired server so a new one can adopt this device.
    #[arg(long)]
    reset: bool,
}

/// Keys that persist across restarts.
struct State {
    identity: Identity,
    /// The server that adopted this device, if any.
    pinned_server: Option<Vec<u8>>,
    pin_path: PathBuf,
}

impl State {
    fn load(dir: &Path, reset: bool) -> anyhow::Result<Self> {
        let identity = Identity::load_or_create(&dir.join("identity.key"))
            .with_context(|| format!("loading device key from {}", dir.display()))?;
        let pin_path = dir.join("server.pub");
        if reset {
            match std::fs::remove_file(&pin_path) {
                Ok(()) => info!("forgot the paired server"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).context("removing pinned server key"),
            }
        }
        let pinned_server = match std::fs::read(&pin_path) {
            Ok(key) => Some(key),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).context("reading pinned server key"),
        };
        Ok(Self {
            identity,
            pinned_server,
            pin_path,
        })
    }

    fn pin_server(&mut self, key: &[u8]) -> anyhow::Result<()> {
        noise::write_private(&self.pin_path, key).context("saving pinned server key")?;
        self.pinned_server = Some(key.to_vec());
        info!(server = %noise::fingerprint(key), "paired with server");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args = Args::parse();
    let device_id = match &args.device_id {
        Some(id) => id.clone(),
        None => default_device_id(args.profile)?,
    };
    let state_dir = match &args.state_dir {
        Some(dir) => dir.clone(),
        None => default_state_dir(&device_id),
    };
    let mut state = State::load(&state_dir, args.reset)?;
    info!(
        %device_id, profile = ?args.profile,
        fingerprint = %noise::fingerprint(state.identity.public_key()),
        paired = state.pinned_server.is_some(),
        "starting Alosa agent"
    );

    let started = Instant::now();
    let mut backoff = Duration::from_secs(1);
    loop {
        let session = async {
            let url = match &args.server {
                Some(url) => url.clone(),
                None => discover_server().await?,
            };
            run_session(&url, &args, &device_id, &mut state, started).await
        };
        match session.await {
            // A session that ran normally resets the backoff.
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => warn!("{e:#}"),
        }
        info!("reconnecting in {backoff:?}");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn run_session(
    url: &str,
    args: &Args,
    device_id: &str,
    state: &mut State,
    started: Instant,
) -> anyhow::Result<()> {
    info!(url, "connecting");
    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .with_context(|| format!("connecting to {url}"))?;
    let (mut sink, mut stream) = ws.split();

    let (server_key, mut sender, mut receiver) = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        handshake(&state.identity, &mut sink, &mut stream),
    )
    .await
    .context("timed out during handshake")??;
    if let Some(pinned) = &state.pinned_server {
        if *pinned != server_key {
            bail!(
                "server at {url} is not the server this device is paired with \
                 (expected {}, got {}). Run with --reset to pair with a new server.",
                noise::fingerprint(pinned),
                noise::fingerprint(&server_key),
            );
        }
    }

    let hello = v1::DeviceMessage {
        msg: Some(v1::device_message::Msg::Hello(v1::Hello {
            device_id: device_id.to_string(),
            model: args.profile.model().to_string(),
            firmware_version: env!("CARGO_PKG_VERSION").to_string(),
            name: args.name.clone().unwrap_or_default(),
            capabilities: Some(args.profile.capabilities()),
            protocol_version: PROTOCOL_VERSION,
        })),
    };
    send(&mut sink, &mut sender, &hello).await?;

    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
    heartbeat.tick().await; // the first tick fires immediately; skip it

    loop {
        tokio::select! {
            frame = stream.next() => {
                let bytes = match frame {
                    None => { info!("server closed the connection"); return Ok(()); }
                    Some(frame) => match frame? {
                        Message::Binary(bytes) => bytes,
                        Message::Close(_) => { info!("server closed the connection"); return Ok(()); }
                        _ => continue,
                    },
                };
                let plaintext = receiver.decrypt(&bytes).context("decrypting message")?;
                let msg = v1::ServerMessage::decode(plaintext.as_slice()).context("decoding ServerMessage")?;
                match msg.msg {
                    Some(v1::server_message::Msg::Welcome(w)) => {
                        info!(
                            server_version = %w.server_version, adopted = w.adopted,
                            server = %noise::fingerprint(&server_key), "connected to server"
                        );
                        if w.adopted && state.pinned_server.is_none() {
                            state.pin_server(&server_key)?;
                        }
                        if !w.adopted {
                            info!(
                                fingerprint = %noise::fingerprint(state.identity.public_key()),
                                "waiting to be adopted in Home Assistant"
                            );
                        }
                        let secs = u64::from(w.heartbeat_interval_s.max(1));
                        heartbeat = tokio::time::interval(Duration::from_secs(secs));
                        heartbeat.tick().await;
                    }
                    Some(v1::server_message::Msg::Command(cmd)) => {
                        let result = handle_command(cmd);
                        send(&mut sink, &mut sender, &v1::DeviceMessage {
                            msg: Some(v1::device_message::Msg::CommandResult(result)),
                        }).await?;
                    }
                    None => warn!("empty message from server"),
                }
            }
            _ = heartbeat.tick() => {
                send(&mut sink, &mut sender, &v1::DeviceMessage {
                    msg: Some(v1::device_message::Msg::Heartbeat(v1::Heartbeat {
                        uptime_s: started.elapsed().as_secs(),
                    })),
                }).await?;
            }
        }
    }
}

fn handle_command(cmd: v1::Command) -> v1::CommandResult {
    let (ok, error) = match cmd.cmd {
        Some(v1::command::Cmd::Identify(identify)) => {
            // TODO: blink LEDs / play a chime once this runs on real hardware.
            info!(
                duration_s = identify.duration_s,
                "*** IDENTIFY: this is me! ***"
            );
            (true, String::new())
        }
        None => (false, "unknown command (newer server?)".to_string()),
    };
    v1::CommandResult {
        command_id: cmd.command_id,
        ok,
        error,
    }
}

/// Runs the initiator side of the Noise handshake. Returns the server's
/// public key and the encrypted channel.
async fn handshake(
    identity: &Identity,
    sink: &mut SplitSink<Ws, Message>,
    stream: &mut SplitStream<Ws>,
) -> anyhow::Result<(Vec<u8>, Sender, Receiver)> {
    let mut hs = Handshake::initiator(identity)?;
    sink.send(Message::Binary(hs.write()?.into())).await?;
    let reply = loop {
        match stream
            .next()
            .await
            .context("server closed during handshake")??
        {
            Message::Binary(bytes) => break bytes,
            Message::Close(_) => bail!("server closed during handshake"),
            _ => continue,
        }
    };
    hs.read(&reply).context("handshake message 2")?;
    sink.send(Message::Binary(hs.write()?.into())).await?;
    Ok(hs.finish()?)
}

async fn send(
    sink: &mut SplitSink<Ws, Message>,
    sender: &mut Sender,
    msg: &v1::DeviceMessage,
) -> anyhow::Result<()> {
    let sealed = sender.encrypt(&msg.encode_to_vec())?;
    sink.send(Message::Binary(sealed.into())).await?;
    Ok(())
}

/// Browses mDNS for the Alosa server and returns its device URL.
async fn discover_server() -> anyhow::Result<String> {
    info!("looking for Alosa server via mDNS");
    let daemon = ServiceDaemon::new()?;
    let receiver = daemon.browse(MDNS_SERVICE_TYPE)?;
    let found = tokio::time::timeout(DISCOVERY_TIMEOUT, async {
        while let Ok(event) = receiver.recv_async().await {
            if let ServiceEvent::ServiceResolved(info) = event {
                let addr = info.get_addresses().iter().find(|a| a.is_ipv4()).copied();
                if let Some(addr) = addr {
                    return Some(format!("ws://{addr}:{}{DEVICE_WS_PATH}", info.get_port()));
                }
            }
        }
        None
    })
    .await;
    let _ = daemon.shutdown();
    match found {
        Ok(Some(url)) => {
            info!(url, "found server");
            Ok(url)
        }
        _ => bail!("no Alosa server found via mDNS (pass --server to set it manually)"),
    }
}

fn default_state_dir(device_id: &str) -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("alosa-agent").join(device_id)
}

/// Stable ID from /etc/machine-id, so the device keeps its identity across
/// reboots. The profile is included so one test machine can run several.
fn default_device_id(profile: Profile) -> anyhow::Result<String> {
    let machine_id = std::fs::read_to_string("/etc/machine-id")
        .context("reading /etc/machine-id (pass --device-id instead)")?;
    let short: String = machine_id.trim().chars().take(12).collect();
    Ok(format!("{}-{short}", profile.model()))
}
