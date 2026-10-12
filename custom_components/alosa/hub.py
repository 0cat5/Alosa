"""Keeps the connection to the Alosa server alive and tracks device state."""

from __future__ import annotations

import asyncio
from collections.abc import Callable
import logging
from typing import Any

from homeassistant.config_entries import SOURCE_INTEGRATION_DISCOVERY, ConfigEntry
from homeassistant.const import CONF_HOST, CONF_PORT, CONF_TOKEN
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers import discovery_flow
from homeassistant.helpers.aiohttp_client import async_get_clientsession

from .client import AlosaClient, AlosaError, InvalidAuth
from .const import API_PATH, DOMAIN

_LOGGER = logging.getLogger(__name__)

MAX_BACKOFF = 60

type Device = dict[str, Any]


class AlosaHub:
    """Owns the API client, reconnects it, and fans out device updates."""

    def __init__(self, hass: HomeAssistant, entry: ConfigEntry) -> None:
        self.hass = hass
        self.entry = entry
        self.client = AlosaClient(
            async_get_clientsession(hass),
            f"ws://{entry.data[CONF_HOST]}:{entry.data[CONF_PORT]}{API_PATH}",
            # Entries from before API tokens have none; the server's 401 then
            # starts the reauth flow, which asks for it.
            entry.data.get(CONF_TOKEN, ""),
        )
        self.devices: dict[str, Device] = {}
        self._new_device_listeners: list[Callable[[Device], None]] = []
        self._update_listeners: dict[str, list[Callable[[], None]]] = {}
        self._listen_task: asyncio.Task[None] | None = None
        self.client.subscribe(self._on_event)

    @property
    def connected(self) -> bool:
        return self.client.connected

    async def async_connect(self) -> None:
        """Connect and load the device list. Raises CannotConnect or InvalidAuth."""
        await self.client.connect()
        # listen() is what reads replies, so it has to run before call().
        self._listen_task = asyncio.create_task(self.client.listen())
        try:
            devices: list[Device] = await self.client.call("devices/list")
        except AlosaError:
            await self.async_close()
            raise
        for device in devices:
            self._update_device(device)

    async def async_run(self) -> None:
        """Background task: wait for disconnects and reconnect with backoff."""
        backoff = 1
        while True:
            if self._listen_task is not None:
                await self._listen_task
            _LOGGER.warning("Lost connection to the Alosa server, reconnecting")
            self._notify_all()
            while True:
                await asyncio.sleep(backoff)
                try:
                    await self.async_connect()
                except InvalidAuth:
                    _LOGGER.error("The Alosa server rejected the API token")
                    self.entry.async_start_reauth(self.hass)
                    return
                except AlosaError as err:
                    _LOGGER.debug("Reconnect failed: %s", err)
                    backoff = min(backoff * 2, MAX_BACKOFF)
                    continue
                _LOGGER.info("Reconnected to the Alosa server")
                backoff = 1
                self._notify_all()
                break

    async def async_close(self) -> None:
        await self.client.close()
        if self._listen_task is not None:
            self._listen_task.cancel()

    @callback
    def add_new_device_listener(self, listener: Callable[[Device], None]) -> Callable[[], None]:
        """Called once for each adopted device, including ones already known."""
        self._new_device_listeners.append(listener)
        for device in self.devices.values():
            if device["adopted"]:
                listener(device)
        return lambda: self._new_device_listeners.remove(listener)

    @callback
    def add_update_listener(
        self, device_id: str, listener: Callable[[], None]
    ) -> Callable[[], None]:
        listeners = self._update_listeners.setdefault(device_id, [])
        listeners.append(listener)
        return lambda: listeners.remove(listener)

    async def async_identify(self, device_id: str) -> None:
        await self.client.call("device/identify", device_id=device_id)

    async def async_adopt(self, device_id: str) -> None:
        await self.client.call("device/adopt", device_id=device_id)

    async def async_remove(self, device_id: str) -> None:
        await self.client.call("device/remove", device_id=device_id)

    @callback
    def _on_event(self, event: str, data: dict[str, Any]) -> None:
        if event == "device_updated":
            self._update_device(data)
        elif event == "device_removed":
            self.devices.pop(data["id"], None)
            for listener in list(self._update_listeners.get(data["id"], [])):
                listener()

    @callback
    def _update_device(self, device: Device) -> None:
        old = self.devices.get(device["id"])
        self.devices[device["id"]] = device
        if device["adopted"] and not (old and old["adopted"]):
            for listener in list(self._new_device_listeners):
                listener(device)
        if not device["adopted"] and device["online"]:
            self._offer_adoption(device)
        for listener in list(self._update_listeners.get(device["id"], [])):
            listener()

    @callback
    def _offer_adoption(self, device: Device) -> None:
        """Show the device under "Discovered" so the user can adopt it."""
        if self.hass.config_entries.flow.async_progress_by_handler(
            DOMAIN, match_context={"unique_id": device["id"]}
        ):
            return
        discovery_flow.async_create_flow(
            self.hass,
            DOMAIN,
            context={"source": SOURCE_INTEGRATION_DISCOVERY},
            data={
                "entry_id": self.entry.entry_id,
                "device_id": device["id"],
                "name": device["name"],
                "model": device["model"],
                "fingerprint": device["fingerprint"],
            },
        )

    @callback
    def _notify_all(self) -> None:
        for listeners in self._update_listeners.values():
            for listener in list(listeners):
                listener()
