# Alosa

Open-source smart speakers, displays and audio bridges for Home Assistant. Fully local.

Alosa devices are adopted and configured by the **Alosa server** (a Home Assistant add-on) and appear in Home Assistant through the **Alosa integration**. Media playback and multiroom audio come from [Music Assistant](https://music-assistant.io) and [Snapcast](https://github.com/badaix/snapcast); voice comes from Home Assistant Assist. Alosa handles the control plane (adoption, configuration, rooms, groups, display content, updates) and never carries the audio itself.

```
Home Assistant ── Alosa integration (Python) ──JSON/WebSocket + API token── Alosa server (Rust, add-on)
                                                                                 │
                                                  Noise-encrypted protobuf/WebSocket, found via mDNS
                                                                                 │
                                                   Alosa devices (Rust agent on Linux, Rust firmware on ESP32)
```

## Security model

- **Device ↔ server** traffic is encrypted and mutually authenticated with the [Noise protocol](https://noiseprotocol.org) (`Noise_XX_25519_ChaChaPoly_BLAKE2s`, the same family ESPHome uses). Each device and the server has its own key; there are no certificates.
- **Adoption is pairing.** A new device shows up in Home Assistant under *Discovered* with its key fingerprint. Adopting it pins keys on both sides: the server rejects any other device that claims the same ID, and the device refuses any other server.
- **Removing** a device in Home Assistant (*Delete* on the device page) unpins it. A device that still has power reconnects and shows up for adoption again.
- **Home Assistant ↔ server** uses an API token, generated on first start. The add-on hands it to Home Assistant automatically through Supervisor discovery.
- Keys and the token live in the server's data dir (`/data` in the add-on, so Home Assistant backups include them). If you lose them, devices have to be reset (`alosa-agent --reset`) and adopted again.

## Repository layout

| Path | What it is |
|---|---|
| `protocol/` | Device protocol (`.proto`), the source of truth for device ↔ server messages |
| `crates/alosa-protocol` | Rust types generated from the protocol, shared by server and devices |
| `crates/alosa-server` | The control server |
| `crates/alosa-agent` | Device agent for Linux-class devices (Raspberry Pi etc.) |
| `integration/custom_components/alosa` | Home Assistant integration |
| `addon/alosa` | Home Assistant add-on packaging for the server |

## Try it locally

Requires Rust (`rustup`).

```sh
cargo build

# 1. Start the server (creates ./data with its key and API token)
./target/debug/alosa-server

# 2. In other terminals, start some simulated devices (they find the server via mDNS)
./target/debug/alosa-agent --profile speaker --name "Kitchen Speaker"
./target/debug/alosa-agent --profile eink-clock --device-id bath-clock-1 --name "Bathroom Clock"
```

Profiles: `connect`, `speaker`, `display`, `eink-clock`. Each one announces a different set of capabilities.

Each agent keeps its key in `~/.local/state/alosa-agent/<device id>/` (change with `--state-dir`). `--reset` forgets the paired server. For quick experiments without Home Assistant, start the server with `--auto-adopt true`.

**On a Raspberry Pi:** build on the Pi with `cargo build --release -p alosa-agent`, or cross-compile for `aarch64-unknown-linux-gnu`, then run `alosa-agent --profile connect`.

**In Home Assistant:** copy `integration/custom_components/alosa` into your HA `config/custom_components/` and restart.
- With the add-on, Home Assistant finds the server by itself; confirm it under *Settings → Devices & services → Discovered*.
- With a standalone server, add the **Alosa** integration manually using the server's host, port (default `8765`) and the token from `data/api_token`.

New devices then appear under *Discovered*. Adopt them there, and each one gets a connectivity sensor and an Identify button.

## Server API (for the integration)

WebSocket at `/api`, JSON messages, `Authorization: Bearer <token>` header. See `crates/alosa-server/src/api_ws.rs` for the command list.

## Known gaps (phase 1)

- **API traffic isn't encrypted.** The token protects the API, but the connection between Home Assistant and a standalone server on another machine is plain WebSocket. With the add-on it never leaves the host.
- **Pairing is trust-on-first-use.** Comparing fingerprints is optional; a QR-code pairing flow for shipped hardware is planned.
- **Add-on images aren't published yet.** The add-on Dockerfile builds from the repo root (`docker build -f addon/alosa/Dockerfile .`); CI to publish images per architecture is still to do.

## License

Apache-2.0
