# Alosa

The Alosa server adopts, configures and monitors Alosa devices (speakers,
displays, audio bridges) and connects them to Home Assistant.

## Setup

1. Start the add-on.
2. Home Assistant finds it automatically: confirm it under
   **Settings → Devices & services → Discovered**. (This needs the Alosa
   integration, installed through HACS.)
3. Power on an Alosa device on the same network. It appears under
   **Discovered** with a key fingerprint. Check that it matches the one the
   device shows (or `sudo alosa-device status` prints), then adopt it.

Speakers then show up in Music Assistant on their own.

## Options

- **Adopt new devices automatically**: skips step 3's approval. Leave it off
  unless you're developing.
- **Log level**: set to `debug` when reporting a problem.

## Backups

Everything the server knows (adopted devices, its key, the API token) lives in
the add-on's data, so Home Assistant backups include it. If you restore onto a
fresh install without a backup, every device has to be reset
(`sudo alosa-device reset`) and adopted again.

## Network

The add-on uses the host network so devices can find it with mDNS, and listens
on port 8765. Devices on a different subnet or VLAN need the server address set
by hand (`ALOSA_SERVER` in `/etc/alosa-agent.conf` on the device).
