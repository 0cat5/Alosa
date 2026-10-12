"""Device buttons: Identify (the device announces itself with LEDs, a chime
or its screen) and Restart (restarts the device software)."""

from __future__ import annotations

from homeassistant.components.button import ButtonDeviceClass, ButtonEntity
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant, callback
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback

from . import AlosaConfigEntry
from .client import AlosaError
from .entity import AlosaEntity
from .hub import Device


async def async_setup_entry(
    hass: HomeAssistant,
    entry: AlosaConfigEntry,
    async_add_entities: AddConfigEntryEntitiesCallback,
) -> None:
    hub = entry.runtime_data

    @callback
    def add_device(device: Device) -> None:
        async_add_entities(
            [
                AlosaIdentifyButton(hub, device, "identify"),
                AlosaRestartButton(hub, device, "restart"),
            ]
        )

    entry.async_on_unload(hub.add_new_device_listener(add_device))


class AlosaIdentifyButton(AlosaEntity, ButtonEntity):
    _attr_device_class = ButtonDeviceClass.IDENTIFY
    _attr_entity_category = EntityCategory.CONFIG

    async def async_press(self) -> None:
        try:
            await self.hub.async_identify(self.device_id)
        except AlosaError as err:
            raise HomeAssistantError(f"Identify failed: {err}") from err


class AlosaRestartButton(AlosaEntity, ButtonEntity):
    _attr_device_class = ButtonDeviceClass.RESTART
    _attr_entity_category = EntityCategory.CONFIG

    async def async_press(self) -> None:
        try:
            await self.hub.async_restart(self.device_id)
        except AlosaError as err:
            raise HomeAssistantError(f"Restart failed: {err}") from err
