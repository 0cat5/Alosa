"""Diagnostics: the "Download diagnostics" button on the integration and on
each device, for bug reports. Secrets and network addresses are redacted."""

from __future__ import annotations

from typing import Any

from homeassistant.components.diagnostics import async_redact_data
from homeassistant.const import CONF_TOKEN
from homeassistant.core import HomeAssistant
from homeassistant.helpers.device_registry import DeviceEntry

from . import AlosaConfigEntry
from .const import DOMAIN
from .hub import AlosaHub

TO_REDACT = {CONF_TOKEN, "public_key", "address"}


def _server(hub: AlosaHub | None) -> dict[str, Any]:
    if hub is None:
        return {"loaded": False}
    return {"loaded": True, "connected": hub.connected, "info": hub.client.server_info}


async def async_get_config_entry_diagnostics(
    hass: HomeAssistant, entry: AlosaConfigEntry
) -> dict[str, Any]:
    hub: AlosaHub | None = getattr(entry, "runtime_data", None)
    return {
        "entry": async_redact_data(dict(entry.data), TO_REDACT),
        "server": _server(hub),
        "devices": async_redact_data(list(hub.devices.values()) if hub else [], TO_REDACT),
    }


async def async_get_device_diagnostics(
    hass: HomeAssistant, entry: AlosaConfigEntry, device: DeviceEntry
) -> dict[str, Any]:
    hub: AlosaHub | None = getattr(entry, "runtime_data", None)
    device_id = next((i for d, i in device.identifiers if d == DOMAIN), None)
    data = hub.devices.get(device_id) if hub and device_id else None
    return {
        "server": _server(hub),
        "device": async_redact_data(data, TO_REDACT) if data else None,
    }
