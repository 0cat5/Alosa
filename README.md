# Alosa

Open-source smart speakers, displays and audio bridges for Home Assistant. Fully local.

Alosa devices are adopted and configured by the **Alosa server** (a Home Assistant add-on) and appear in Home Assistant through the **Alosa integration**. Media playback and multiroom audio come from [Music Assistant](https://music-assistant.io) over [Sendspin](https://www.sendspin-audio.com), its native synchronized-audio protocol; voice comes from Home Assistant Assist. Alosa handles the control plane (adoption, configuration, rooms, display content, updates) and never carries the audio itself.

```
Home Assistant ── Alosa integration (Python) ──JSON/WebSocket + API token── Alosa server (Rust, add-on)
                                                                                 │
                                                  Noise-encrypted protobuf/WebSocket, found via mDNS
                                                                                 │
                                                   Alosa devices (Rust agent on Linux, Rust firmware on ESP32)
                                                                                 │
                                                       Sendspin player (sendspin-cli), run by the agent
                                                                                 │
                                                             Music Assistant (finds it via mDNS)
```

## Install

Three parts, each updated through its usual channel. They share one version number; see [CHANGELOG.md](CHANGELOG.md).

**1. The server (add-on).** In Home Assistant: *Settings → Add-ons → Add-on store → ⋮ → Repositories*, add `https://github.com/0cat5/Alosa`, then install and start **Alosa**.

**2. The integration.** In HACS: *⋮ → Custom repositories*, add `https://github.com/0cat5/Alosa` as an *Integration*, install **Alosa**, and restart Home Assistant. It then finds the add-on by itself: confirm it under *Settings → Devices & services → Discovered*. (Without HACS: unzip `alosa-integration-<version>.zip` from a [release](https://github.com/0cat5/Alosa/releases) into `config/custom_components/alosa/`.)

**3. Each device** (Raspberry Pi OS or another Debian-based Linux, 64- or 32-bit):

```sh
curl -fsSL https://raw.githubusercontent.com/0cat5/Alosa/main/scripts/alosa-device.sh \
  | sudo sh -s install --profile connect --name "Living Room"
```

Profiles: `connect` (audio bridge), `speaker`, `display`, `eink-clock`. The installer sets up the agent, the Sendspin player it's tested with, the libraries they need, and a systemd service. The device then appears in Home Assistant under *Discovered*: adopt it, pick its **Audio output**, and it shows up in Music Assistant.

> **While the repository is private**, Home Assistant, HACS and the one-line installer can't reach it. Until then, install the integration by hand and install devices from release files: `sudo sh alosa-device.sh install --from <dir with the release files>`. The add-on images on GHCR also need to be made public (*Package settings → Change visibility*) for Home Assistant to pull them.

## Update

- **Server**: Home Assistant shows an update for the add-on, with the changelog.
- **Integration**: HACS shows an update.
- **Devices**: `sudo alosa-device update` updates the agent and its Sendspin player together. If the new agent doesn't stay running, it puts the old one back. `sudo alosa-device auto-update on` does this weekly.

## Maintenance

| What | How |
|---|---|
| Is a device OK? | Its **Connectivity** and **Audio status** entities in Home Assistant, or `sudo alosa-device status` on the device (versions, service state, pairing, recent log) |
| Find a device | **Identify** button |
| Restart a device's software | **Restart** button, or `sudo systemctl restart alosa-agent` |
| Rename a device | Rename it in Home Assistant; the new name also shows in Music Assistant |
| Report a problem | *Download diagnostics* on the integration or device page (secrets are redacted), plus the add-on log or `journalctl -u alosa-agent` |
| Move a device to another server | *Delete* it in Home Assistant, or `sudo alosa-device reset` on the device, then adopt it again |
| Remove the software | `sudo alosa-device uninstall` (`--purge` also deletes the device's key and settings) |

The server's state (adopted devices, its key, the API token) is in the add-on's data, so Home Assistant backups include it. The add-on restarts itself if it stops responding.

## Audio

Devices with speakers run [`sendspin-cli`](https://github.com/Sendspin/sendspin-cpp-cli), the official Sendspin player, as a child process of the agent. Once a device is adopted:

- The player advertises itself and **Music Assistant adds it automatically**, under the device's name and listed as an Alosa product. Grouping, volume, sync delay and EQ are all Music Assistant settings.
- **Audio output** (a select entity in Home Assistant) picks which sound card, DAC or HDMI port it plays through. The server pushes the choice to the device.
- **Audio status** (a sensor) shows whether the player is stopped, waiting for Music Assistant, idle, playing, or in error.
- If the player crashes, the agent restarts it. If the Alosa server goes down, the device keeps playing on its last config.
- Unadopted or removed devices don't play anything.

## Security model

- **Device ↔ server** traffic is encrypted and mutually authenticated with the [Noise protocol](https://noiseprotocol.org) (`Noise_XX_25519_ChaChaPoly_BLAKE2s`, the same family ESPHome uses). Each device and the server has its own key; there are no certificates.
- **Adoption is pairing.** A new device shows up in Home Assistant under *Discovered* with its key fingerprint (`sudo alosa-device status` shows the device's side). Adopting it pins keys on both sides: the server rejects any other device that claims the same ID, and the device refuses any other server.
- **Removing** a device in Home Assistant unpins it. A device that still has power reconnects and shows up for adoption again.
- **Home Assistant ↔ server** uses an API token, generated on first start. The add-on hands it to Home Assistant through Supervisor discovery.
- **Updates** are checked against the release's SHA-256 checksums, and a new agent binary must run before it replaces the old one.

## Development

| Path | What it is |
|---|---|
| `protocol/` | Device protocol (`.proto`), the source of truth for device ↔ server messages |
| `crates/alosa-protocol` | Rust types generated from the protocol, plus the Noise channel, shared by server and devices |
| `crates/alosa-server` | The control server |
| `crates/alosa-agent` | Device agent for Linux-class devices; `systemd/` has its service files, `sendspin-cli.version` the player version it's tested with |
| `custom_components/alosa` | Home Assistant integration |
| `addon/alosa` | Home Assistant add-on packaging for the server |
| `scripts/` | `alosa-device.sh` (device installer), `package.sh` (release build), `version.sh`, `e2e.sh` |
| `tests/e2e` | End-to-end tests |

### Run it locally

Requires Rust (`rustup`).

```sh
cargo build

# 1. Start the server (creates ./data with its key and API token)
./target/debug/alosa-server

# 2. In other terminals, start some simulated devices (they find the server via mDNS)
./target/debug/alosa-agent --profile speaker --name "Kitchen Speaker"
./target/debug/alosa-agent --profile eink-clock --device-id bath-clock-1 --name "Bathroom Clock"
```

Each agent keeps its key in `~/.local/state/alosa-agent/<device id>/` (change with `--state-dir`); `--info` prints its identity and pairing. For quick experiments without Home Assistant, start the server with `--auto-adopt true`. For audio, install `sendspin-cli` or point `--sendspin-bin` at it, and give each simulated audio device its own `--audio-port`.

To use a local server from Home Assistant, add the **Alosa** integration manually with the server's host, port (`8765`) and the token from `data/api_token`.

### Tests

```sh
cargo test          # unit tests
scripts/e2e.sh      # real server + agents + sendspin-cli, driven through the integration's client
```

CI runs both, plus clippy, ruff, shellcheck, Home Assistant's `hassfest`, and a release build for every architecture.

### Releasing

```sh
scripts/version.sh set 0.2.0     # Cargo.toml, the integration manifest and the add-on config
# add a "## 0.2.0" section to CHANGELOG.md
git commit -am "Release 0.2.0" && git tag v0.2.0 && git push --follow-tags
```

The tag triggers `.github/workflows/release.yml`: static agent binaries (x86_64, arm64, armv7, armv6), the integration zip and checksums go to a GitHub release, and the add-on images go to GHCR. `scripts/package.sh` builds the same release files locally into `dist/`.

### Server API (for the integration)

WebSocket at `/api`, JSON messages, `Authorization: Bearer <token>` header. See `crates/alosa-server/src/api_ws.rs` for the command list.

## Known gaps

- **API traffic isn't encrypted.** The token protects the API, but the connection between Home Assistant and a standalone server on another machine is plain WebSocket. With the add-on it never leaves the host.
- **Pairing is trust-on-first-use.** Comparing fingerprints is optional; a QR-code pairing flow for shipped hardware is planned.
- **Sendspin is a technical preview**, and `sendspin-cli` 0.3.0 connects to Music Assistant without encryption (Music Assistant's "Allow legacy clients" setting, on by default, must stay on). Newer `sendspin-cli` builds add Sendspin pairing; adopting a device should then pair it with Music Assistant automatically.
- **Device buttons, LEDs and line-in** aren't wired up yet. `sendspin-cli` already has the controls (play/pause/next/volume) for the buttons to use.
- **Updates from Home Assistant**: devices update with `alosa-device`, not yet from an update entity in Home Assistant.

## License

Apache-2.0
