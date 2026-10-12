"""Audio output select: which sound card an Alosa device plays through."""

from __future__ import annotations

from typing import Any

from homeassistant.components.select import SelectEntity
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback

from . import AlosaConfigEntry
from .client import AlosaError
from .entity import AlosaEntity
from .hub import Device

# What the device plays through when nothing was chosen.
DEFAULT_OUTPUT = "default"


def audio_out(device: Device | None) -> dict[str, Any] | None:
    """The device's audio output capability, if it has one."""
    if device is None:
        return None
    return (device.get("capabilities") or {}).get("audio_out")


async def async_setup_entry(
    hass: HomeAssistant,
    entry: AlosaConfigEntry,
    async_add_entities: AddConfigEntryEntitiesCallback,
) -> None:
    hub = entry.runtime_data

    @callback
    def add_device(device: Device) -> None:
        if audio_out(device) is not None:
            async_add_entities([AlosaAudioOutputSelect(hub, device, "audio_output")])

    entry.async_on_unload(hub.add_new_device_listener(add_device))


class AlosaAudioOutputSelect(AlosaEntity, SelectEntity):
    _attr_entity_category = EntityCategory.CONFIG
    _attr_translation_key = "audio_output"

    def _outputs(self) -> dict[str, str]:
        """Option label -> output id. Labels are the device's descriptions,
        with the id added where two descriptions would collide."""
        outputs = (audio_out(self.device) or {}).get("outputs", [])
        descriptions = [o["description"] or o["id"] for o in outputs]
        labels: dict[str, str] = {}
        for output, description in zip(outputs, descriptions, strict=True):
            label = description
            if descriptions.count(description) > 1:
                label = f"{description} ({output['id']})"
            labels[label] = output["id"]
        return labels

    @property
    def available(self) -> bool:
        # A device that couldn't list its outputs has nothing to choose from.
        return super().available and bool(self._outputs())

    @property
    def options(self) -> list[str]:
        return list(self._outputs())

    @property
    def current_option(self) -> str | None:
        device = self.device
        if device is None:
            return None
        selected = device.get("settings", {}).get("audio_output") or DEFAULT_OUTPUT
        for label, output_id in self._outputs().items():
            if output_id == selected:
                return label
        return None

    async def async_select_option(self, option: str) -> None:
        output_id = self._outputs()[option]
        try:
            await self.hub.async_set_audio_output(self.device_id, output_id)
        except AlosaError as err:
            raise HomeAssistantError(f"Could not change the audio output: {err}") from err
