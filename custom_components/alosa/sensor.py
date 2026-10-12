"""Audio status sensor: whether an Alosa device's player is running and in use."""

from __future__ import annotations

from typing import Any

from homeassistant.components.sensor import SensorDeviceClass, SensorEntity
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback

from . import AlosaConfigEntry
from .entity import AlosaEntity
from .hub import Device
from .select import audio_out

# stopped: not running (e.g. audio turned off)
# waiting: running, but no Music Assistant (or other Sendspin server) connected yet
# idle:    connected, nothing playing
# playing: connected and playing
# error:   crashed or can't start; the device keeps retrying
AUDIO_STATES = ["stopped", "waiting", "idle", "playing", "error"]


async def async_setup_entry(
    hass: HomeAssistant,
    entry: AlosaConfigEntry,
    async_add_entities: AddConfigEntryEntitiesCallback,
) -> None:
    hub = entry.runtime_data

    @callback
    def add_device(device: Device) -> None:
        if audio_out(device) is not None:
            async_add_entities([AlosaAudioStatusSensor(hub, device, "audio_status")])

    entry.async_on_unload(hub.add_new_device_listener(add_device))


class AlosaAudioStatusSensor(AlosaEntity, SensorEntity):
    _attr_device_class = SensorDeviceClass.ENUM
    _attr_entity_category = EntityCategory.DIAGNOSTIC
    _attr_options = AUDIO_STATES
    _attr_translation_key = "audio_status"

    def _audio(self) -> dict[str, Any] | None:
        device = self.device
        if device is None:
            return None
        return (device.get("status") or {}).get("audio")

    @property
    def native_value(self) -> str | None:
        audio = self._audio()
        if audio is None:
            return None
        if audio["engine"] == "failed":
            return "error"
        if audio["engine"] != "running":
            return "stopped"
        if not audio["server_name"]:
            return "waiting"
        return "playing" if audio["playback"] == "playing" else "idle"

    @property
    def extra_state_attributes(self) -> dict[str, Any]:
        audio = self._audio() or {}
        return {
            "server": audio.get("server_name") or None,
            "error": audio.get("error") or None,
        }
