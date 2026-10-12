# Changelog

All Alosa components (server add-on, Home Assistant integration, device agent)
share one version number. The add-on store shows this file when an update is
available.

## 0.1.0 (unreleased)

First release.

- **Server** (Home Assistant add-on): adopts and configures Alosa devices,
  announces itself to Home Assistant, restarts itself if it stops responding.
- **Integration**: new devices appear under *Discovered* for adoption. Each
  device gets a connectivity sensor, Identify and Restart buttons, and (for
  speakers) an Audio output select and Audio status sensor. Renaming a device
  in Home Assistant renames it in Music Assistant too. Diagnostics can be
  downloaded for bug reports.
- **Device agent**: encrypted, mutually authenticated connection to the server
  (Noise protocol, keys pinned at adoption). Speakers play through Music
  Assistant via Sendspin and keep playing if the server restarts.
- **Device installer** (`alosa-device`): installs, updates (with automatic
  rollback if the new version fails to start), and optionally auto-updates
  devices.
