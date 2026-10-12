//! The audio engine: runs Sendspin's player (`sendspin-cli`) so Music
//! Assistant can play to this device.
//!
//! The player advertises itself over mDNS and Music Assistant connects to it;
//! grouping, volume and sync all happen between the two. Alosa's part is to
//! start it only once the device is adopted, configure it (name, output),
//! restart it if it dies, and report what it's doing.

use std::{
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use alosa_protocol::v1::{self, AudioEngineState, PlaybackState};
use nix::{
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::watch,
};
use tracing::{info, warn};

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// An engine that ran this long before dying counts as healthy, so the next
/// crash restarts it quickly instead of continuing the backoff.
const HEALTHY_RUN: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
const STATUS_INTERVAL: Duration = Duration::from_secs(2);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// How to run the engine. Fixed for the life of the agent.
pub struct EngineSettings {
    pub bin: PathBuf,
    pub device_id: String,
    pub product_name: String,
    /// Port the player's own WebSocket server listens on (Sendspin default 8928).
    pub port: u16,
    pub control_socket: PathBuf,
    pub state_dir: PathBuf,
}

/// What the engine should be doing. Changing any field restarts it.
#[derive(Clone, Debug, PartialEq)]
struct Desired {
    name: String,
    output: String,
}

pub struct AudioEngine {
    desired: watch::Sender<Option<Desired>>,
    status: watch::Receiver<v1::AudioStatus>,
}

impl AudioEngine {
    /// Starts the supervisor. The engine itself stays stopped until
    /// [`AudioEngine::apply`] gives it a config.
    pub fn start(settings: EngineSettings) -> Self {
        let settings = Arc::new(settings);
        let (desired_tx, desired_rx) = watch::channel(None);
        let (status_tx, status_rx) = watch::channel(stopped());
        tokio::spawn(supervise(settings.clone(), desired_rx, status_tx.clone()));
        tokio::spawn(poll_status(settings, status_tx));
        Self {
            desired: desired_tx,
            status: status_rx,
        }
    }

    /// Applies the server's config. `None` (or a config without audio) stops
    /// the engine. Only a real change restarts it.
    pub fn apply(&self, config: Option<&v1::DeviceConfig>) {
        let desired = config.and_then(|c| {
            c.audio.as_ref().map(|audio| Desired {
                name: c.name.clone(),
                output: if audio.output.is_empty() {
                    "default".into()
                } else {
                    audio.output.clone()
                },
            })
        });
        self.desired.send_if_modified(|current| {
            let changed = *current != desired;
            *current = desired;
            changed
        });
    }

    pub fn status(&self) -> watch::Receiver<v1::AudioStatus> {
        self.status.clone()
    }

    /// Stops the engine and waits for it to exit, so the agent never leaves
    /// an orphaned player behind.
    pub async fn shutdown(&self) {
        self.apply(None);
        let mut status = self.status.clone();
        let stopped = status.wait_for(|s| s.engine == AudioEngineState::Stopped as i32);
        let _ = tokio::time::timeout(STOP_TIMEOUT + Duration::from_secs(1), stopped).await;
    }
}

fn stopped() -> v1::AudioStatus {
    v1::AudioStatus {
        engine: AudioEngineState::Stopped as i32,
        playback: PlaybackState::Idle as i32,
        ..Default::default()
    }
}

fn running() -> v1::AudioStatus {
    v1::AudioStatus {
        engine: AudioEngineState::Running as i32,
        playback: PlaybackState::Idle as i32,
        ..Default::default()
    }
}

fn failed(error: String) -> v1::AudioStatus {
    v1::AudioStatus {
        engine: AudioEngineState::Failed as i32,
        error,
        playback: PlaybackState::Idle as i32,
        ..Default::default()
    }
}

/// Keeps the engine process matching `desired`: starts, stops, restarts on
/// config changes, and restarts with backoff when it dies.
async fn supervise(
    settings: Arc<EngineSettings>,
    mut desired: watch::Receiver<Option<Desired>>,
    status: watch::Sender<v1::AudioStatus>,
) {
    let mut backoff = MIN_BACKOFF;
    loop {
        let Some(want) = desired.borrow_and_update().clone() else {
            status.send_replace(stopped());
            if desired.changed().await.is_err() {
                return;
            }
            continue;
        };

        let started = Instant::now();
        let exit = match spawn(&settings, &want) {
            Ok(mut child) => {
                info!(name = %want.name, output = %want.output, "audio engine started");
                status.send_replace(running());
                tokio::select! {
                    exit = child.wait() => exit,
                    changed = desired.changed() => {
                        if desired.borrow().is_some() {
                            info!("audio config changed, restarting engine");
                        } else {
                            info!("stopping audio engine");
                        }
                        stop(&mut child).await;
                        if changed.is_err() {
                            return;
                        }
                        backoff = MIN_BACKOFF;
                        continue;
                    }
                }
            }
            Err(e) => Err(e),
        };

        let error = match exit {
            Ok(code) => describe_exit(code),
            Err(e) => format!("could not start {}: {e}", settings.bin.display()),
        };
        warn!("audio engine stopped: {error}; retrying in {backoff:?}");
        status.send_replace(failed(error));
        if started.elapsed() >= HEALTHY_RUN {
            backoff = MIN_BACKOFF;
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            changed = desired.changed() => if changed.is_err() { return },
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

fn spawn(settings: &EngineSettings, want: &Desired) -> std::io::Result<Child> {
    std::fs::create_dir_all(&settings.state_dir)?;
    let mut child = Command::new(&settings.bin)
        // Ignore any sendspin-cli config file on the host: Alosa owns these settings.
        .args(["--config", "/dev/null"])
        .args(["--id", &settings.device_id])
        .args(["--name", &want.name])
        .args(["--manufacturer", "Alosa"])
        .args(["--product-name", &settings.product_name])
        .args(["--output", &want.output])
        .args(["--port", &settings.port.to_string()])
        .arg("--state-dir")
        .arg(&settings.state_dir)
        .arg("--control-socket")
        .arg(&settings.control_socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    // Forward the engine's log lines into ours.
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                info!(target: "sendspin", "{line}");
            }
        });
    }
    Ok(child)
}

/// Asks the engine to exit cleanly, so it can say goodbye to the server and
/// withdraw its mDNS advertisement, then kills it if it doesn't.
async fn stop(child: &mut Child) {
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
        if tokio::time::timeout(STOP_TIMEOUT, child.wait())
            .await
            .is_ok()
        {
            return;
        }
        warn!("audio engine ignored SIGTERM, killing it");
    }
    let _ = child.kill().await;
}

fn describe_exit(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("sendspin-cli exited with status {code}"),
        None => "sendspin-cli was killed by a signal".to_string(),
    }
}

/// While the engine runs, asks it what it's doing and publishes changes.
async fn poll_status(settings: Arc<EngineSettings>, status: watch::Sender<v1::AudioStatus>) {
    let mut tick = tokio::time::interval(STATUS_INTERVAL);
    loop {
        tick.tick().await;
        if status.borrow().engine != AudioEngineState::Running as i32 {
            continue;
        }
        // The engine may still be starting up; just try again next tick.
        let Ok(text) = run_engine_command(&settings, &["status"]).await else {
            continue;
        };
        let (server_name, playback) = parse_status(&text);
        status.send_if_modified(|s| {
            // The engine may have died while we were asking.
            if s.engine != AudioEngineState::Running as i32 {
                return false;
            }
            let changed = s.server_name != server_name || s.playback != playback as i32;
            s.server_name = server_name;
            s.playback = playback as i32;
            changed
        });
    }
}

async fn run_engine_command(settings: &EngineSettings, args: &[&str]) -> anyhow::Result<String> {
    let output = tokio::time::timeout(
        COMMAND_TIMEOUT,
        Command::new(&settings.bin)
            .args(args)
            .arg("--control-socket")
            .arg(&settings.control_socket)
            .stdin(Stdio::null())
            .output(),
    )
    .await??;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Reads `sendspin-cli status` output, e.g.
///
/// ```text
/// server: Music Assistant (connected)
/// stream: receiving
/// ```
fn parse_status(text: &str) -> (String, PlaybackState) {
    let mut server_name = String::new();
    let mut playback = PlaybackState::Idle;
    for line in text.lines() {
        if let Some(server) = line.strip_prefix("server: ") {
            if let Some(name) = server.strip_suffix(" (connected)") {
                server_name = name.to_string();
            }
        } else if line.strip_prefix("stream: ") == Some("receiving") {
            playback = PlaybackState::Playing;
        }
    }
    (server_name, playback)
}

/// The engine's name and version, e.g. "sendspin-cli 0.3.0", or `None` if it
/// can't be run.
pub async fn engine_version(bin: &std::path::Path) -> Option<String> {
    let output = tokio::time::timeout(COMMAND_TIMEOUT, Command::new(bin).arg("--version").output())
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    output
        .status
        .success()
        .then(|| text.lines().next().unwrap_or("").trim().to_string())
}

/// Lists the outputs the engine can play through, for the user to choose
/// from. Empty if `sendspin-cli` can't be run.
pub async fn list_outputs(bin: &std::path::Path) -> Vec<v1::AudioOutput> {
    let output =
        match tokio::time::timeout(COMMAND_TIMEOUT, Command::new(bin).arg("-l").output()).await {
            Ok(Ok(output)) if output.status.success() => output,
            Ok(Ok(output)) => {
                warn!(
                    "{} -l failed: {}",
                    bin.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                return Vec::new();
            }
            Ok(Err(e)) => {
                warn!(
                    "can't run {} ({e}); is sendspin-cli installed? Audio won't work.",
                    bin.display()
                );
                return Vec::new();
            }
            Err(_) => {
                warn!("{} -l timed out", bin.display());
                return Vec::new();
            }
        };
    let mut outputs = parse_outputs(&String::from_utf8_lossy(&output.stdout));
    outputs.push(v1::AudioOutput {
        id: "null".into(),
        description: "No sound".into(),
    });
    outputs
}

/// Parses the ALSA, PulseAudio and PipeWire sections of `sendspin-cli -l`.
/// Each section is a header line, then entries (2-space indent) each followed
/// by detail lines (6-space indent, the first being a description), ending at
/// a blank line.
fn parse_outputs(text: &str) -> Vec<v1::AudioOutput> {
    #[derive(Clone, Copy)]
    enum Section {
        Alsa,
        Pulse,
        PipeWire,
    }

    let mut outputs: Vec<v1::AudioOutput> = Vec::new();
    let mut section = None;
    // Whether the latest entry still needs its description line.
    let mut want_description = false;

    for line in text.lines() {
        if line.trim().is_empty() {
            section = None;
            continue;
        }
        if !line.starts_with(' ') {
            section = if line.starts_with("ALSA PCMs") {
                Some(Section::Alsa)
            } else if line.starts_with("PulseAudio sinks") {
                Some(Section::Pulse)
            } else if line.starts_with("PipeWire audio sink nodes") {
                Some(Section::PipeWire)
            } else {
                None
            };
            continue;
        }
        let Some(section) = section else { continue };

        if let Some(detail) = line.strip_prefix("      ") {
            if want_description {
                if let Some(last) = outputs.last_mut() {
                    let detail = detail.trim().trim_end_matches(" *");
                    last.description = match section {
                        Section::Alsa => detail.to_string(),
                        Section::Pulse => format!("{detail} (PulseAudio)"),
                        Section::PipeWire => format!("{detail} (PipeWire)"),
                    };
                }
                want_description = false;
            }
        } else if let Some(entry) = line.strip_prefix("  ") {
            let name = entry.trim().trim_end_matches("(server default)").trim();
            // Multichannel variants of the same card would only clutter the list.
            if name.starts_with("surround") || name.starts_with("front:") {
                want_description = false;
                continue;
            }
            let id = match section {
                Section::Alsa => name.to_string(),
                Section::Pulse => format!("pulse:{name}"),
                Section::PipeWire => format!("pipewire:{name}"),
            };
            outputs.push(v1::AudioOutput {
                description: id.clone(),
                id,
            });
            want_description = true;
        }
    }
    outputs
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "\
Output devices (-o):
  null      discard audio; needs no sound card at all

ALSA PCMs on this host (any of these names can follow -o):
  default
      Default ALSA Output (currently PipeWire Media Server)
      rates:    22050 32000 44100 48000
  hdmi:CARD=NVidia,DEV=0
      HDA NVidia, HDMI 0 *
      HDMI Audio Output
  front:CARD=Generic,DEV=0
      HD-Audio Generic, ALC1220 Analog
  hw:CARD=sndrpihifiberry,DEV=0
      snd_rpi_hifiberry_dacplus, HiFiBerry DAC+ HiFi pcm512x-hifi-0

Hardware PCMs also accept the short hw:<card>,<device> and
plughw:<card>,<device> forms.

PulseAudio sinks on this host (-o pulse:<sink>):
  alsa_output.analog-stereo  (server default)
      Mackie Monitors
      running at s32le 2ch 48000Hz

  Every sink above accepts all of:
      rates:    22050 32000

PipeWire audio sink nodes on this host (-o pipewire:<node>):
  alsa_output.analog-stereo
      Mackie Monitors
";

    #[test]
    fn parses_outputs() {
        let outputs: Vec<(String, String)> = parse_outputs(LIST)
            .into_iter()
            .map(|o| (o.id, o.description))
            .collect();
        assert_eq!(
            outputs,
            [
                (
                    "default",
                    "Default ALSA Output (currently PipeWire Media Server)"
                ),
                ("hdmi:CARD=NVidia,DEV=0", "HDA NVidia, HDMI 0"),
                (
                    "hw:CARD=sndrpihifiberry,DEV=0",
                    "snd_rpi_hifiberry_dacplus, HiFiBerry DAC+ HiFi pcm512x-hifi-0"
                ),
                (
                    "pulse:alsa_output.analog-stereo",
                    "Mackie Monitors (PulseAudio)"
                ),
                (
                    "pipewire:alsa_output.analog-stereo",
                    "Mackie Monitors (PipeWire)"
                ),
            ]
            .map(|(a, b)| (a.to_string(), b.to_string()))
        );
    }

    #[test]
    fn parses_status() {
        let playing = "name: Kitchen\nserver: Music Assistant (connected)\nstream: receiving\n";
        assert_eq!(
            parse_status(playing),
            ("Music Assistant".to_string(), PlaybackState::Playing)
        );
        let idle = "name: Kitchen\nserver: not connected\nstream: idle\n";
        assert_eq!(parse_status(idle), (String::new(), PlaybackState::Idle));
    }
}
