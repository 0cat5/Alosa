//! The device registry: every known device, which ones are connected, and the
//! event stream the Home Assistant integration subscribes to.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use alosa_protocol::{noise, v1};
use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, Notify, RwLock};
use tracing::{info, warn};

use crate::capabilities::Capabilities;

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub model: String,
    pub firmware_version: String,
    pub adopted: bool,
    /// Runtime state only; every device starts offline after a server restart.
    #[serde(default, skip_deserializing)]
    pub online: bool,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// The device's Noise public key (hex), pinned when it is first seen.
    #[serde(default)]
    pub public_key: String,
    /// Short form of `public_key`, shown in Home Assistant for verification.
    #[serde(default)]
    pub fingerprint: String,
}

/// Events pushed to API clients (the Home Assistant integration).
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum Event {
    DeviceUpdated(Box<Device>),
    DeviceRemoved { id: String },
}

/// A live device connection. `id` distinguishes a reconnect from the
/// connection it replaces, so the old one can't mark the device offline.
struct Connection {
    id: u64,
    tx: mpsc::Sender<v1::ServerMessage>,
    /// Signals the connection's task to hang up (device removed).
    kick: Arc<Notify>,
}

pub struct Registry {
    devices: RwLock<HashMap<String, Device>>,
    connections: RwLock<HashMap<String, Connection>>,
    pending_commands: Mutex<HashMap<u64, oneshot::Sender<v1::CommandResult>>>,
    next_id: AtomicU64,
    events: broadcast::Sender<Event>,
    store_path: PathBuf,
    auto_adopt: bool,
}

impl Registry {
    pub async fn load(data_dir: PathBuf, auto_adopt: bool) -> anyhow::Result<Arc<Self>> {
        tokio::fs::create_dir_all(&data_dir)
            .await
            .with_context(|| format!("creating data dir {}", data_dir.display()))?;
        let store_path = data_dir.join("devices.json");

        let devices: HashMap<String, Device> = match tokio::fs::read(&store_path).await {
            Ok(bytes) => {
                let list: Vec<Device> = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", store_path.display()))?;
                list.into_iter().map(|d| (d.id.clone(), d)).collect()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e).context("reading device store"),
        };
        info!(count = devices.len(), path = %store_path.display(), "loaded device store");

        let (events, _) = broadcast::channel(256);
        Ok(Arc::new(Self {
            devices: RwLock::new(devices),
            connections: RwLock::new(HashMap::new()),
            pending_commands: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            events,
            store_path,
            auto_adopt,
        }))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub async fn list(&self) -> Vec<Device> {
        let mut list: Vec<Device> = self.devices.read().await.values().cloned().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// Registers a device that completed the handshake and said Hello.
    /// Returns its record, the connection id to pass back to
    /// [`Registry::disconnect`], and a signal that fires if the server wants
    /// the connection closed.
    pub async fn connect(
        &self,
        hello: v1::Hello,
        public_key: &[u8],
        address: String,
        tx: mpsc::Sender<v1::ServerMessage>,
    ) -> anyhow::Result<(Device, u64, Arc<Notify>)> {
        if hello.device_id.trim().is_empty() {
            bail!("Hello has an empty device_id");
        }
        let conn_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let key_hex = noise::to_hex(public_key);

        let device = {
            let mut devices = self.devices.write().await;
            if let Some(existing) = devices.get(&hello.device_id) {
                // An adopted device's key is pinned. A different key means a
                // different physical device is claiming this ID.
                // Devices adopted before keys existed have none stored; pin on
                // their next connection.
                if existing.adopted
                    && !existing.public_key.is_empty()
                    && existing.public_key != key_hex
                {
                    bail!(
                        "device {} presented a different key than the one pinned at adoption \
                         (expected {}, got {}). If this device was reset, remove it in Home \
                         Assistant and adopt it again.",
                        hello.device_id,
                        existing.fingerprint,
                        noise::fingerprint(public_key),
                    );
                }
            }
            let device = devices.entry(hello.device_id.clone()).or_insert_with(|| {
                info!(device_id = %hello.device_id, model = %hello.model, "new device discovered");
                Device {
                    id: hello.device_id.clone(),
                    name: if hello.name.is_empty() {
                        hello.device_id.clone()
                    } else {
                        hello.name.clone()
                    },
                    model: String::new(),
                    firmware_version: String::new(),
                    adopted: self.auto_adopt,
                    online: false,
                    address: None,
                    capabilities: Capabilities::default(),
                    public_key: String::new(),
                    fingerprint: String::new(),
                }
            });
            // Hardware facts always come from the device; the name belongs to the user.
            device.model = hello.model;
            device.firmware_version = hello.firmware_version;
            device.capabilities = hello.capabilities.unwrap_or_default().into();
            device.address = Some(address);
            device.public_key = key_hex;
            device.fingerprint = noise::fingerprint(public_key);
            device.online = true;
            device.clone()
        };

        let kick = Arc::new(Notify::new());
        let conn = Connection {
            id: conn_id,
            tx,
            kick: kick.clone(),
        };
        if let Some(old) = self
            .connections
            .write()
            .await
            .insert(device.id.clone(), conn)
        {
            info!(device_id = %device.id, old_conn = old.id, "device reconnected, replacing old connection");
            old.kick.notify_one();
        }

        self.persist().await;
        self.emit(Event::DeviceUpdated(Box::new(device.clone())));
        Ok((device, conn_id, kick))
    }

    pub async fn disconnect(&self, device_id: &str, conn_id: u64) {
        {
            let mut conns = self.connections.write().await;
            match conns.get(device_id) {
                Some(c) if c.id == conn_id => {
                    conns.remove(device_id);
                }
                // A newer connection has taken over; leave the device online.
                _ => return,
            }
        }
        let device = {
            let mut devices = self.devices.write().await;
            let Some(device) = devices.get_mut(device_id) else {
                return;
            };
            device.online = false;
            device.clone()
        };
        info!(device_id, "device offline");
        self.emit(Event::DeviceUpdated(Box::new(device)));
    }

    pub async fn adopt(&self, device_id: &str) -> anyhow::Result<Device> {
        let device = {
            let mut devices = self.devices.write().await;
            let device = devices
                .get_mut(device_id)
                .ok_or_else(|| anyhow!("unknown device {device_id}"))?;
            device.adopted = true;
            device.clone()
        };
        info!(device_id, "device adopted");
        self.persist().await;
        self.emit(Event::DeviceUpdated(Box::new(device.clone())));
        // Tell the device right away so it doesn't wait for a reconnect.
        if let Some(conn) = self.connections.read().await.get(device_id) {
            let _ = conn.tx.send(welcome(true)).await;
        }
        Ok(device)
    }

    /// Forgets a device and disconnects it. If it is still powered on it will
    /// reconnect and show up as a new, unadopted device.
    pub async fn remove(&self, device_id: &str) -> anyhow::Result<()> {
        if self.devices.write().await.remove(device_id).is_none() {
            bail!("unknown device {device_id}");
        }
        if let Some(conn) = self.connections.write().await.remove(device_id) {
            conn.kick.notify_one();
        }
        info!(device_id, "device removed");
        self.persist().await;
        self.emit(Event::DeviceRemoved {
            id: device_id.to_string(),
        });
        Ok(())
    }

    /// Sends a command to a connected device and waits for its result.
    pub async fn send_command(
        &self,
        device_id: &str,
        cmd: v1::command::Cmd,
    ) -> anyhow::Result<v1::CommandResult> {
        match self.devices.read().await.get(device_id) {
            Some(device) if device.adopted => {}
            Some(_) => bail!("device {device_id} has not been adopted"),
            None => bail!("unknown device {device_id}"),
        }
        let tx = self
            .connections
            .read()
            .await
            .get(device_id)
            .map(|c| c.tx.clone())
            .ok_or_else(|| anyhow!("device {device_id} is not connected"))?;

        let command_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (done_tx, done_rx) = oneshot::channel();
        self.pending_commands
            .lock()
            .await
            .insert(command_id, done_tx);

        let msg = v1::ServerMessage {
            msg: Some(v1::server_message::Msg::Command(v1::Command {
                command_id,
                cmd: Some(cmd),
            })),
        };
        if tx.send(msg).await.is_err() {
            self.pending_commands.lock().await.remove(&command_id);
            bail!("device {device_id} disconnected");
        }

        match tokio::time::timeout(COMMAND_TIMEOUT, done_rx).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => bail!("command {command_id} was dropped"),
            Err(_) => {
                self.pending_commands.lock().await.remove(&command_id);
                bail!("device {device_id} did not respond within {COMMAND_TIMEOUT:?}")
            }
        }
    }

    pub async fn complete_command(&self, result: v1::CommandResult) {
        match self
            .pending_commands
            .lock()
            .await
            .remove(&result.command_id)
        {
            Some(done) => {
                let _ = done.send(result);
            }
            None => warn!(
                command_id = result.command_id,
                "result for unknown or expired command"
            ),
        }
    }

    fn emit(&self, event: Event) {
        // Err only means nobody is subscribed right now, which is fine.
        let _ = self.events.send(event);
    }

    /// Writes the device list to disk. Written to a temp file and renamed so
    /// a crash mid-write can't leave a corrupt store behind.
    async fn persist(&self) {
        let list = self.list().await;
        let result = async {
            let json = serde_json::to_vec_pretty(&list)?;
            let tmp = self.store_path.with_extension("json.tmp");
            tokio::fs::write(&tmp, json).await?;
            tokio::fs::rename(&tmp, &self.store_path).await?;
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = result {
            warn!("failed to persist device store: {e:#}");
        }
    }
}

pub fn welcome(adopted: bool) -> v1::ServerMessage {
    v1::ServerMessage {
        msg: Some(v1::server_message::Msg::Welcome(v1::Welcome {
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            adopted,
            heartbeat_interval_s: HEARTBEAT_INTERVAL.as_secs() as u32,
        })),
    }
}
