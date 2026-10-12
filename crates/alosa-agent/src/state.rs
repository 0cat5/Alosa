//! What the agent remembers across restarts, all kept in the state dir:
//!
//! - `identity.key`: this device's key (created on first run)
//! - `server.pub`: the server that adopted it (pinned at adoption)
//! - `config.pb`: the last config that server sent, so the device keeps
//!   working (e.g. playing music) while the server is unreachable

use std::{
    io,
    path::{Path, PathBuf},
};

use alosa_protocol::{
    noise::{self, Identity},
    v1,
};
use anyhow::Context;
use prost::Message as _;
use tracing::{info, warn};

pub struct State {
    pub identity: Identity,
    /// The server that adopted this device, if any.
    pub pinned_server: Option<Vec<u8>>,
    /// The latest config from that server, if any.
    pub config: Option<v1::DeviceConfig>,
    dir: PathBuf,
}

impl State {
    /// Loads the state. `reset` forgets the paired server and its config,
    /// like a factory reset (the device keeps its own key).
    pub fn load(dir: &Path, reset: bool) -> anyhow::Result<Self> {
        let identity = Identity::load_or_create(&dir.join("identity.key"))
            .with_context(|| format!("loading device key from {}", dir.display()))?;
        if reset {
            remove_if_exists(&dir.join("server.pub")).context("removing pinned server key")?;
            remove_if_exists(&dir.join("config.pb")).context("removing saved config")?;
            info!("forgot the paired server");
        }
        let pinned_server =
            read_if_exists(&dir.join("server.pub")).context("reading pinned server key")?;
        let config = read_config(dir)?;
        Ok(Self {
            identity,
            pinned_server,
            config,
            dir: dir.to_path_buf(),
        })
    }

    pub fn pin_server(&mut self, key: &[u8]) -> anyhow::Result<()> {
        noise::write_private(&self.dir.join("server.pub"), key)
            .context("saving pinned server key")?;
        self.pinned_server = Some(key.to_vec());
        info!(server = %noise::fingerprint(key), "paired with server");
        Ok(())
    }

    /// Saves (or with `None`, forgets) the server's config.
    pub fn save_config(&mut self, config: Option<v1::DeviceConfig>) -> anyhow::Result<()> {
        let path = self.dir.join("config.pb");
        match &config {
            Some(config) => {
                noise::write_private(&path, &config.encode_to_vec()).context("saving config")?
            }
            None => remove_if_exists(&path).context("removing saved config")?,
        }
        self.config = config;
        Ok(())
    }
}

/// A read-only look at the state dir, for `--info`. Unlike [`State::load`]
/// it creates nothing, so it's safe to run as root against a service's state.
pub struct Snapshot {
    pub public_key: Option<Vec<u8>>,
    pub pinned_server: Option<Vec<u8>>,
    pub config: Option<v1::DeviceConfig>,
}

impl Snapshot {
    pub fn read(dir: &Path) -> anyhow::Result<Self> {
        let public_key = match Identity::load(&dir.join("identity.key")) {
            Ok(identity) => Some(identity.public_key().to_vec()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).context("reading device key"),
        };
        Ok(Self {
            public_key,
            pinned_server: read_if_exists(&dir.join("server.pub"))
                .context("reading pinned server key")?,
            config: read_config(dir)?,
        })
    }
}

fn read_config(dir: &Path) -> anyhow::Result<Option<v1::DeviceConfig>> {
    let Some(bytes) = read_if_exists(&dir.join("config.pb")).context("reading saved config")?
    else {
        return Ok(None);
    };
    match v1::DeviceConfig::decode(bytes.as_slice()) {
        Ok(config) => Ok(Some(config)),
        Err(e) => {
            warn!("ignoring unreadable saved config: {e}");
            Ok(None)
        }
    }
}

fn read_if_exists(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}
