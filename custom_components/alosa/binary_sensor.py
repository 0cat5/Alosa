"""Connectivity sensor for each Alosa device."""

from __future__ import annotations

from homeassistant.components.binary_sensor import BinarySensorDeviceClass, BinarySensorEntity
from homeassistant.const import EntityCategory
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback

from . import AlosaConfigEntry
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
        async_add_entities([AlosaConnectivity(hub, device, "connectivity")])

    entry.async_on_unload(hub.add_new_device_listener(add_device))


class AlosaConnectivity(AlosaEntity, BinarySensorEntity):
    _attr_device_class = BinarySensorDeviceClass.CONNECTIVITY
    _attr_entity_category = EntityCategory.DIAGNOSTIC

    @property
    def available(self) -> bool:
        # Stays available while the device is offline, so HA can show "Disconnected".
        return self.hub.connected and self.device is not None

    @property
    def is_on(self) -> bool:
        device = self.device
        return device is not None and device["online"]
