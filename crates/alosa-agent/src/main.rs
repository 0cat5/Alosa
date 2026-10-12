//! Alosa device agent for Linux-class devices.
//!
//! Finds the server (mDNS or --server), runs the encrypted handshake, says
//! Hello, sends heartbeats and status, applies config, and handles commands.
//! Reconnects forever with backoff, because a device should recover on its own
//! from server restarts and network blips.
//!
//! Pairing: the device trusts whichever server adopts it, and from then on
//! only that server (its key is pinned in the state dir). `--reset` forgets
//! the pinned server, like a factory reset.
//!
//! Audio: devices with speakers run Sendspin's player (see `audio.rs`) once
//! adopted, and keep it running from the saved config while the server is
//! unreachable.

mod audio;
mod profiles;
mod state;

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
use tokio::{net::TcpStream, sync::watch};
use tokio_tungstenite::{tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::{
    audio::{AudioEngine, EngineSettings},
    profiles::Profile,
    state::{Snapshot, State},
};

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
    #[arg(long, env = "ALOSA_PROFILE", value_enum, default_value_t = Profile::Connect)]
    profile: Profile,

    /// Unique device ID. Derived from /etc/machine-id if not set.
    #[arg(long, env = "ALOSA_DEVICE_ID")]
    device_id: Option<String>,

    /// Suggested friendly name.
    #[arg(long, env = "ALOSA_DEVICE_NAME")]
    name: Option<String>,

    /// Where the device key, pinned server key and saved config are kept.
    /// Defaults to ~/.local/state/alosa-agent/<device id>.
    #[arg(long, env = "ALOSA_STATE_DIR")]
    state_dir: Option<PathBuf>,

    /// Forget the paired server so a new one can adopt this device.
    #[arg(long)]
    reset: bool,

    /// Print this device's identity, pairing and saved config, then exit.
    /// Changes nothing.
    #[arg(long)]
    info: bool,

    /// The Sendspin player that plays audio.
    #[arg(long, env = "ALOSA_SENDSPIN_BIN", default_value = "sendspin-cli")]
    sendspin_bin: PathBuf,

    /// Port the Sendspin player listens on for Music Assistant. Give each
    /// simulated device on one machine its own.
    #[arg(long, env = "ALOSA_AUDIO_PORT", default_value_t = 8928)]
    audio_port: u16,

    /// Control socket for the Sendspin player. Defaults to
    /// $XDG_RUNTIME_DIR/alosa-sendspin-<port>.sock, or
    /// /run/alosa-agent/sendspin-<port>.sock when that isn't set.
    #[arg(long, env = "ALOSA_CONTROL_SOCKET")]
    control_socket: Option<PathBuf>,
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
    if args.info {
        return print_info(&args, &device_id, &state_dir).await;
    }
    let mut state = State::load(&state_dir, args.reset)?;
    info!(
        %device_id, profile = ?args.profile,
        fingerprint = %noise::fingerprint(state.identity.public_key()),
        paired = state.pinned_server.is_some(),
        "starting Alosa agent"
    );

    let (engine, outputs) = if args.profile.has_audio() {
        let outputs = audio::list_outputs(&args.sendspin_bin).await;
        let engine = AudioEngine::start(EngineSettings {
            bin: args.sendspin_bin.clone(),
            device_id: device_id.clone(),
            product_name: args.profile.product_name().to_string(),
            port: args.audio_port,
            control_socket: args
                .control_socket
                .clone()
                .unwrap_or_else(|| default_control_socket(args.audio_port)),
            state_dir: state_dir.join("sendspin"),
        });
        // A paired device picks up where it left off, even if the server is down.
        if state.pinned_server.is_some() {
            engine.apply(state.config.as_ref());
        }
        (Some(engine), outputs)
    } else {
        (None, Vec::new())
    };
    let mut capabilities = args.profile.capabilities(outputs);
    if let Some(audio) = capabilities.audio_out.as_mut() {
        audio.engine = audio::engine_version(&args.sendspin_bin)
            .await
            .unwrap_or_default();
    }

    let mut session = Session {
        args: &args,
        device_id: &device_id,
        state: &mut state,
        capabilities: &capabilities,
        engine: engine.as_ref(),
        started: Instant::now(),
        restart_requested: false,
    };
    // run_forever only returns when the server asks for a restart.
    let restart = tokio::select! {
        _ = session.run_forever() => true,
        _ = shutdown_signal() => {
            info!("shutting down");
            false
        }
    };
    if let Some(engine) = &engine {
        engine.shutdown().await;
    }
    if restart {
        return Err(reexec());
    }
    Ok(())
}

/// Replaces this process with a fresh start of the agent, with the same
/// arguments minus `--reset` (a restart must not forget the paired server).
/// Only returns if that fails.
fn reexec() -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let mut args = std::env::args_os();
    // argv[0] is the path that was run (e.g. /usr/local/bin/alosa-agent), so a
    // binary replaced by an update is picked up.
    let program = args.next().unwrap_or_else(|| "alosa-agent".into());
    info!("restarting");
    let err = std::process::Command::new(program)
        .args(args.filter(|a| a != "--reset"))
        .exec();
    anyhow::Error::from(err).context("restarting the agent")
}

/// `--info`: what this device is and who it belongs to.
async fn print_info(args: &Args, device_id: &str, state_dir: &Path) -> anyhow::Result<()> {
    let snapshot = Snapshot::read(state_dir)?;
    let fingerprint = |key: &Option<Vec<u8>>| key.as_deref().map(noise::fingerprint);
    println!("device id:    {device_id}");
    println!("model:        {}", args.profile.model());
    println!("version:      {}", env!("CARGO_PKG_VERSION"));
    println!("state dir:    {}", state_dir.display());
    println!(
        "fingerprint:  {}",
        fingerprint(&snapshot.public_key)
            .unwrap_or_else(|| "none yet (created on first start)".into())
    );
    println!(
        "paired with:  {}",
        fingerprint(&snapshot.pinned_server)
            .map(|fp| format!("server {fp}"))
            .unwrap_or_else(|| "nothing (waiting to be adopted)".into())
    );
    match &snapshot.config {
        Some(config) => {
            println!("name:         {}", config.name);
            if args.profile.has_audio() {
                let output = match &config.audio {
                    Some(audio) if audio.output.is_empty() => "default",
                    Some(audio) => &audio.output,
                    None => "off",
                };
                println!("audio output: {output}");
            }
        }
        None => println!("config:       none saved"),
    }
    if args.profile.has_audio() {
        let engine = audio::engine_version(&args.sendspin_bin).await;
        println!(
            "audio engine: {}",
            engine.unwrap_or_else(|| format!("{} not found", args.sendspin_bin.display()))
        );
    }
    Ok(())
}

/// Resolves on Ctrl-C or SIGTERM (what systemd sends to stop a service).
async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

/// Everything a connection to the server needs. Lives across reconnects.
struct Session<'a> {
    args: &'a Args,
    device_id: &'a str,
    state: &'a mut State,
    capabilities: &'a v1::Capabilities,
    engine: Option<&'a AudioEngine>,
    started: Instant,
    /// Set when the server asked for a restart; ends `run_forever`.
    restart_requested: bool,
}

impl Session<'_> {
    /// Connects to the server and stays connected, reconnecting with backoff.
    async fn run_forever(&mut self) {
        let mut backoff = Duration::from_secs(1);
        loop {
            let url = match &self.args.server {
                Some(url) => Ok(url.clone()),
                None => discover_server().await,
            };
            match url {
                Ok(url) => match self.run(&url).await {
                    // A session that ran normally resets the backoff.
                    Ok(()) => backoff = Duration::from_secs(1),
                    Err(e) => warn!("{e:#}"),
                },
                Err(e) => warn!("{e:#}"),
            }
            if self.restart_requested {
                return;
            }
            info!("reconnecting in {backoff:?}");
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    async fn run(&mut self, url: &str) -> anyhow::Result<()> {
        info!(url, "connecting");
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .with_context(|| format!("connecting to {url}"))?;
        let (mut sink, mut stream) = ws.split();

        let (server_key, mut sender, mut receiver) = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            handshake(&self.state.identity, &mut sink, &mut stream),
        )
        .await
        .context("timed out during handshake")??;
        if let Some(pinned) = &self.state.pinned_server {
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
                device_id: self.device_id.to_string(),
                model: self.args.profile.model().to_string(),
                firmware_version: env!("CARGO_PKG_VERSION").to_string(),
                name: self.args.name.clone().unwrap_or_default(),
                capabilities: Some(self.capabilities.clone()),
                protocol_version: PROTOCOL_VERSION,
            })),
        };
        send(&mut sink, &mut sender, &hello).await?;

        let mut audio_status = self.engine.map(AudioEngine::status);
        send(&mut sink, &mut sender, &status_message(&mut audio_status)).await?;

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
                            self.on_welcome(&w, &server_key)?;
                            let secs = u64::from(w.heartbeat_interval_s.max(1));
                            heartbeat = tokio::time::interval(Duration::from_secs(secs));
                            heartbeat.tick().await;
                        }
                        Some(v1::server_message::Msg::Config(config)) => self.on_config(config)?,
                        Some(v1::server_message::Msg::Command(cmd)) => {
                            let restart = matches!(cmd.cmd, Some(v1::command::Cmd::Restart(_)));
                            let result = handle_command(cmd);
                            send(&mut sink, &mut sender, &v1::DeviceMessage {
                                msg: Some(v1::device_message::Msg::CommandResult(result)),
                            }).await?;
                            if restart {
                                info!("restart requested by the server");
                                self.restart_requested = true;
                                let _ = sink.send(Message::Close(None)).await;
                                return Ok(());
                            }
                        }
                        None => warn!("empty message from server"),
                    }
                }
                true = status_changed(&mut audio_status) => {
                    send(&mut sink, &mut sender, &status_message(&mut audio_status)).await?;
                }
                _ = heartbeat.tick() => {
                    send(&mut sink, &mut sender, &v1::DeviceMessage {
                        msg: Some(v1::device_message::Msg::Heartbeat(v1::Heartbeat {
                            uptime_s: self.started.elapsed().as_secs(),
                        })),
                    }).await?;
                }
            }
        }
    }

    fn on_welcome(&mut self, welcome: &v1::Welcome, server_key: &[u8]) -> anyhow::Result<()> {
        info!(
            server_version = %welcome.server_version, adopted = welcome.adopted,
            server = %noise::fingerprint(server_key), "connected to server"
        );
        if welcome.adopted && self.state.pinned_server.is_none() {
            self.state.pin_server(server_key)?;
        }
        if !welcome.adopted {
            // Not adopted (or removed): this device does nothing until it is.
            if self.state.config.is_some() {
                info!("no longer adopted, stopping and forgetting config");
            }
            self.state.save_config(None)?;
            if let Some(engine) = self.engine {
                engine.apply(None);
            }
            info!(
                fingerprint = %noise::fingerprint(self.state.identity.public_key()),
                "waiting to be adopted in Home Assistant"
            );
        }
        Ok(())
    }

    fn on_config(&mut self, config: v1::DeviceConfig) -> anyhow::Result<()> {
        info!(name = %config.name, audio = ?config.audio, "received config");
        if let Some(engine) = self.engine {
            engine.apply(Some(&config));
        }
        self.state.save_config(Some(config))
    }
}

fn status_message(audio: &mut Option<watch::Receiver<v1::AudioStatus>>) -> v1::DeviceMessage {
    v1::DeviceMessage {
        msg: Some(v1::device_message::Msg::Status(v1::Status {
            audio: audio.as_mut().map(|rx| rx.borrow_and_update().clone()),
        })),
    }
}

/// Resolves to true when the audio status changes; never resolves for a
/// device without audio.
async fn status_changed(audio: &mut Option<watch::Receiver<v1::AudioStatus>>) -> bool {
    match audio {
        Some(rx) => rx.changed().await.is_ok(),
        None => std::future::pending().await,
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
        // Acknowledged here; the session then shuts down and restarts.
        Some(v1::command::Cmd::Restart(_)) => (true, String::new()),
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

/// Unix socket paths are limited to ~107 bytes, so this lives in the short
/// runtime dir rather than the state dir.
fn default_control_socket(port: u16) -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join(format!("alosa-sendspin-{port}.sock")),
        None => PathBuf::from(format!("/run/alosa-agent/sendspin-{port}.sock")),
    }
}

/// Stable ID from /etc/machine-id, so the device keeps its identity across
/// reboots. The profile is included so one test machine can run several.
fn default_device_id(profile: Profile) -> anyhow::Result<String> {
    let machine_id = std::fs::read_to_string("/etc/machine-id")
        .context("reading /etc/machine-id (pass --device-id instead)")?;
    let short: String = machine_id.trim().chars().take(12).collect();
    Ok(format!("{}-{short}", profile.model()))
}
