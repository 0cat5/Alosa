//! WebSocket endpoint for the Home Assistant integration (`/api`). Speaks JSON.
//! Requires `Authorization: Bearer <api token>` on the upgrade request.
//!
//! Requests:  {"id": 1, "command": "devices/list", "args": {...}}
//! Responses: {"id": 1, "result": ...}  or  {"id": 1, "error": "..."}
//! Events:    {"event": "device_updated", "data": {...}}
//!
//! Commands:
//!   server/info                                  -> {"version", "server_id"}
//!   devices/list                                 -> [Device, ...]
//!   device/adopt     {"device_id"}               -> Device
//!   device/remove    {"device_id"}               -> {}
//!   device/rename    {"device_id", "name"}       -> Device
//!   device/restart   {"device_id"}               -> {}   (restarts the device software)
//!   device/configure {"device_id", "audio_output"} -> Device
//!                    (audio_output: an id from the device's outputs, or null for its default)
//!   device/identify  {"device_id", "duration_s"} -> {}

use std::sync::Arc;

use alosa_protocol::v1;
use anyhow::{anyhow, bail};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast::error::RecvError, mpsc};
use tracing::{info, warn};

use crate::registry::Registry;

#[derive(Deserialize)]
struct Request {
    id: u64,
    command: String,
    #[serde(default)]
    args: Value,
}

#[derive(Deserialize)]
struct DeviceArgs {
    device_id: String,
}

#[derive(Deserialize)]
struct RenameArgs {
    device_id: String,
    name: String,
}

#[derive(Deserialize)]
struct ConfigureArgs {
    device_id: String,
    audio_output: Option<String>,
}

#[derive(Deserialize)]
struct IdentifyArgs {
    device_id: String,
    #[serde(default = "default_identify_duration")]
    duration_s: u32,
}

fn default_identify_duration() -> u32 {
    5
}

pub async fn handle(socket: WebSocket, registry: Arc<Registry>, server_id: Arc<str>) {
    info!("API client connected");
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<String>(64);

    let writer = tokio::spawn(async move {
        while let Some(text) = rx.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let _ = tx
        .send(json!({"event": "server_info", "data": server_info(&server_id)}).to_string())
        .await;

    // Subscribe before handling any request so no update is missed between a
    // client's devices/list and its first event.
    let mut events = registry.subscribe();
    let event_tx = tx.clone();
    let forwarder = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let text = serde_json::to_string(&event).expect("events always serialize");
                    if event_tx.send(text).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(n)) => warn!(missed = n, "API client fell behind on events"),
                Err(RecvError::Closed) => break,
            }
        }
    });

    while let Some(Ok(frame)) = stream.next().await {
        let text = match frame {
            Message::Text(text) => text,
            Message::Close(_) => break,
            _ => continue,
        };
        let req: Request = match serde_json::from_str(text.as_str()) {
            Ok(req) => req,
            Err(e) => {
                let _ = tx
                    .send(json!({"error": format!("invalid request: {e}")}).to_string())
                    .await;
                continue;
            }
        };
        // Each request runs on its own task so a slow command (e.g. waiting on
        // a device) doesn't block other requests on this connection.
        let registry = registry.clone();
        let server_id = server_id.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let reply = match dispatch(&registry, &server_id, &req.command, req.args).await {
                Ok(result) => json!({"id": req.id, "result": result}),
                Err(e) => json!({"id": req.id, "error": format!("{e:#}")}),
            };
            let _ = tx.send(reply.to_string()).await;
        });
    }

    forwarder.abort();
    writer.abort();
    info!("API client disconnected");
}

async fn dispatch(
    registry: &Registry,
    server_id: &str,
    command: &str,
    args: Value,
) -> anyhow::Result<Value> {
    match command {
        "server/info" => Ok(server_info(server_id)),
        "devices/list" => Ok(serde_json::to_value(registry.list().await)?),
        "device/adopt" => {
            let args: DeviceArgs = parse_args(args)?;
            Ok(serde_json::to_value(
                registry.adopt(&args.device_id).await?,
            )?)
        }
        "device/rename" => {
            let args: RenameArgs = parse_args(args)?;
            Ok(serde_json::to_value(
                registry.rename(&args.device_id, args.name).await?,
            )?)
        }
        "device/restart" => {
            let args: DeviceArgs = parse_args(args)?;
            let cmd = v1::command::Cmd::Restart(v1::Restart {});
            let result = registry.send_command(&args.device_id, cmd).await?;
            if !result.ok {
                bail!("device reported an error: {}", result.error);
            }
            Ok(json!({}))
        }
        "device/configure" => {
            let args: ConfigureArgs = parse_args(args)?;
            let device = registry
                .configure(&args.device_id, args.audio_output)
                .await?;
            Ok(serde_json::to_value(device)?)
        }
        "device/remove" => {
            let args: DeviceArgs = parse_args(args)?;
            registry.remove(&args.device_id).await?;
            Ok(json!({}))
        }
        "device/identify" => {
            let args: IdentifyArgs = parse_args(args)?;
            let cmd = v1::command::Cmd::Identify(v1::Identify {
                duration_s: args.duration_s,
            });
            let result = registry.send_command(&args.device_id, cmd).await?;
            if !result.ok {
                bail!("device reported an error: {}", result.error);
            }
            Ok(json!({}))
        }
        other => Err(anyhow!("unknown command {other:?}")),
    }
}

fn parse_args<T: serde::de::DeserializeOwned>(args: Value) -> anyhow::Result<T> {
    serde_json::from_value(args).map_err(|e| anyhow!("invalid args: {e}"))
}

/// `server_id` is the fingerprint of the server's key: stable for the life of
/// the install, and what Home Assistant uses to tell servers apart.
fn server_info(server_id: &str) -> Value {
    json!({ "version": env!("CARGO_PKG_VERSION"), "server_id": server_id })
}
