"""Base entity shared by every Alosa platform."""

from __future__ import annotations

from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.entity import Entity

from .const import DOMAIN, MANUFACTURER
from .hub import AlosaHub, Device


class AlosaEntity(Entity):
    """An entity that belongs to one Alosa device and follows its updates."""

    _attr_has_entity_name = True
    _attr_should_poll = False

    def __init__(self, hub: AlosaHub, device: Device, key: str) -> None:
        self.hub = hub
        self.device_id: str = device["id"]
        self._attr_unique_id = f"{self.device_id}_{key}"
        self._attr_device_info = DeviceInfo(
            identifiers={(DOMAIN, self.device_id)},
            name=device["name"],
            manufacturer=MANUFACTURER,
            model=device["model"],
            sw_version=device["firmware_version"],
        )

    @property
    def device(self) -> Device | None:
        """The device's latest state, or None if it was just removed."""
        return self.hub.devices.get(self.device_id)

    @property
    def available(self) -> bool:
        device = self.device
        return self.hub.connected and device is not None and device["online"]

    async def async_added_to_hass(self) -> None:
        self.async_on_remove(
            self.hub.add_update_listener(self.device_id, self.async_write_ha_state)
        )
