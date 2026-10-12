//! WebSocket endpoint that devices connect to (`/device`).
//!
//! Every connection starts with a Noise handshake (see `alosa_protocol::noise`);
//! after that each binary frame is one encrypted protobuf message.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use alosa_protocol::{
    noise::{Handshake, Identity, Receiver, Sender},
    v1, PROTOCOL_VERSION,
};
use anyhow::{bail, Context};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use prost::Message as _;
use tokio::sync::{mpsc, oneshot, Notify};
use tracing::{debug, info, warn};

use crate::registry::{welcome, Registry, HEARTBEAT_INTERVAL};

/// Time allowed for the handshake and Hello together.
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
/// Missing this many heartbeats in a row marks the device offline.
const MISSED_HEARTBEATS: u32 = 3;

pub async fn handle(
    socket: WebSocket,
    addr: SocketAddr,
    registry: Arc<Registry>,
    identity: Arc<Identity>,
) {
    if let Err(e) = run(socket, addr, &registry, &identity).await {
        warn!(%addr, "device connection closed: {e:#}");
    }
}

async fn run(
    socket: WebSocket,
    addr: SocketAddr,
    registry: &Registry,
    identity: &Identity,
) -> anyhow::Result<()> {
    let (mut sink, mut stream) = socket.split();

    let setup = async {
        let (device_key, sender, mut receiver) =
            handshake(identity, &mut sink, &mut stream).await?;
        let hello = match next_message(&mut stream, &mut receiver).await? {
            Some(v1::DeviceMessage {
                msg: Some(v1::device_message::Msg::Hello(hello)),
            }) => hello,
            Some(_) => bail!("first message was not Hello"),
            None => bail!("closed before Hello"),
        };
        anyhow::Ok((device_key, sender, receiver, hello))
    };
    let (device_key, mut sender, mut receiver, hello) = tokio::time::timeout(SETUP_TIMEOUT, setup)
        .await
        .context("timed out during handshake")??;

    if hello.protocol_version != PROTOCOL_VERSION {
        warn!(device_id = %hello.device_id, version = hello.protocol_version, "device speaks a different protocol version");
    }

    let (tx, mut rx) = mpsc::channel::<v1::ServerMessage>(32);
    let (device, conn_id, kick) = registry
        .connect(hello, &device_key, addr.ip().to_string(), tx.clone())
        .await?;
    info!(
        device_id = %device.id, model = %device.model, %addr,
        adopted = device.adopted, fingerprint = %device.fingerprint, "device online"
    );
    tx.send(welcome(device.adopted)).await?;

    // Writer: everything queued for this device is encrypted and sent. When
    // the read side ends, it sends a close frame so the device sees a clean
    // hang-up rather than a reset connection.
    let (close_tx, mut close_rx) = oneshot::channel::<()>();
    let writer = tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                msg = rx.recv() => msg,
                _ = &mut close_rx => None,
            };
            let Some(msg) = msg else {
                let _ = sink.send(Message::Close(None)).await;
                break;
            };
            let sealed = match sender.encrypt(&msg.encode_to_vec()) {
                Ok(sealed) => sealed,
                Err(e) => {
                    warn!("failed to encrypt message: {e}");
                    break;
                }
            };
            if sink.send(Message::Binary(sealed.into())).await.is_err() {
                break;
            }
        }
    });

    let result = read_loop(&mut stream, &mut receiver, registry, &device.id, &kick).await;
    let _ = close_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(1), writer).await;
    registry.disconnect(&device.id, conn_id).await;
    result
}

/// Runs the responder side of the Noise handshake. Returns the device's
/// public key and the encrypted channel.
async fn handshake(
    identity: &Identity,
    sink: &mut SplitSink<WebSocket, Message>,
    stream: &mut SplitStream<WebSocket>,
) -> anyhow::Result<(Vec<u8>, Sender, Receiver)> {
    let mut hs = Handshake::responder(identity)?;
    hs.read(
        &next_frame(stream)
            .await?
            .context("closed during handshake")?,
    )
    .context("handshake message 1")?;
    sink.send(Message::Binary(hs.write()?.into())).await?;
    hs.read(
        &next_frame(stream)
            .await?
            .context("closed during handshake")?,
    )
    .context("handshake message 3")?;
    Ok(hs.finish()?)
}

async fn read_loop(
    stream: &mut SplitStream<WebSocket>,
    receiver: &mut Receiver,
    registry: &Registry,
    device_id: &str,
    kick: &Notify,
) -> anyhow::Result<()> {
    let deadline = HEARTBEAT_INTERVAL * MISSED_HEARTBEATS;
    loop {
        let msg = tokio::select! {
            _ = kick.notified() => {
                info!(device_id, "closing connection at the server's request");
                return Ok(());
            }
            result = tokio::time::timeout(deadline, next_message(stream, receiver)) => match result {
                Err(_) => bail!("no heartbeat for {deadline:?}"),
                Ok(result) => match result? {
                    Some(msg) => msg,
                    None => return Ok(()),
                },
            },
        };
        match msg.msg {
            Some(v1::device_message::Msg::Heartbeat(hb)) => {
                debug!(device_id, uptime_s = hb.uptime_s, "heartbeat");
            }
            Some(v1::device_message::Msg::CommandResult(result)) => {
                registry.complete_command(result).await;
            }
            Some(v1::device_message::Msg::Hello(_)) => warn!(device_id, "ignoring repeated Hello"),
            None => warn!(device_id, "empty message"),
        }
    }
}

/// Reads and decrypts the next message. Returns `None` when the socket closes.
async fn next_message(
    stream: &mut SplitStream<WebSocket>,
    receiver: &mut Receiver,
) -> anyhow::Result<Option<v1::DeviceMessage>> {
    let Some(frame) = next_frame(stream).await? else {
        return Ok(None);
    };
    let plaintext = receiver.decrypt(&frame).context("decrypting message")?;
    Ok(Some(
        v1::DeviceMessage::decode(plaintext.as_slice()).context("decoding DeviceMessage")?,
    ))
}

/// Reads the next binary frame, skipping ping/pong/text frames.
/// Returns `None` when the socket closes.
async fn next_frame(stream: &mut SplitStream<WebSocket>) -> anyhow::Result<Option<Vec<u8>>> {
    while let Some(frame) = stream.next().await {
        match frame? {
            Message::Binary(bytes) => return Ok(Some(bytes.to_vec())),
            Message::Close(_) => return Ok(None),
            _ => continue,
        }
    }
    Ok(None)
}
