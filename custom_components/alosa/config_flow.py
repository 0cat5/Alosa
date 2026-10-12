"""Config flow for Alosa.

Three ways in:
- hassio: the Alosa add-on announces itself with its address and API token.
- user: manual setup (standalone server), token copied from the server's data dir.
- integration_discovery: a new device is waiting to be adopted. This doesn't
  create a config entry; it adopts the device into the existing server entry.
"""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any

import voluptuous as vol

from homeassistant.config_entries import ConfigEntryState, ConfigFlow, ConfigFlowResult
from homeassistant.const import CONF_HOST, CONF_PORT, CONF_TOKEN
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.service_info.hassio import HassioServiceInfo

from .client import AlosaClient, AlosaError, CannotConnect, InvalidAuth
from .const import API_PATH, DEFAULT_PORT, DOMAIN

# The add-on runs on the host network (for mDNS), so Home Assistant reaches it locally.
DEFAULT_HOST = "127.0.0.1"


class AlosaConfigFlow(ConfigFlow, domain=DOMAIN):
    VERSION = 1

    def __init__(self) -> None:
        self._hassio_data: dict[str, Any] = {}
        self._device: dict[str, Any] = {}

    async def _async_server_info(self, data: Mapping[str, Any]) -> dict[str, Any]:
        """Connect with the given settings. Raises CannotConnect or InvalidAuth."""
        client = AlosaClient(
            async_get_clientsession(self.hass),
            f"ws://{data[CONF_HOST]}:{data[CONF_PORT]}{API_PATH}",
            data[CONF_TOKEN],
        )
        try:
            return await client.connect()
        finally:
            await client.close()

    async def async_step_user(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        errors: dict[str, str] = {}
        if user_input is not None:
            try:
                info = await self._async_server_info(user_input)
            except InvalidAuth:
                errors["base"] = "invalid_auth"
            except CannotConnect:
                errors["base"] = "cannot_connect"
            else:
                await self.async_set_unique_id(info["server_id"])
                self._abort_if_unique_id_configured(updates=user_input)
                return self.async_create_entry(title="Alosa", data=user_input)

        schema = vol.Schema(
            {
                vol.Required(CONF_HOST, default=DEFAULT_HOST): str,
                vol.Required(CONF_PORT, default=DEFAULT_PORT): int,
                vol.Required(CONF_TOKEN): str,
            }
        )
        return self.async_show_form(
            step_id="user",
            data_schema=self.add_suggested_values_to_schema(schema, user_input),
            errors=errors,
        )

    async def async_step_hassio(self, discovery_info: HassioServiceInfo) -> ConfigFlowResult:
        config = discovery_info.config
        data = {
            CONF_HOST: config["host"],
            CONF_PORT: config["port"],
            CONF_TOKEN: config["api_token"],
        }
        try:
            info = await self._async_server_info(data)
        except AlosaError:
            return self.async_abort(reason="cannot_connect")
        await self.async_set_unique_id(info["server_id"])
        # Already set up: refresh the address and token (e.g. after a restore).
        self._abort_if_unique_id_configured(updates=data)
        self._hassio_data = data
        return await self.async_step_hassio_confirm()

    async def async_step_hassio_confirm(
        self, user_input: dict[str, Any] | None = None
    ) -> ConfigFlowResult:
        if user_input is not None:
            return self.async_create_entry(title="Alosa", data=self._hassio_data)
        return self.async_show_form(step_id="hassio_confirm")

    async def async_step_reauth(self, entry_data: Mapping[str, Any]) -> ConfigFlowResult:
        return await self.async_step_reauth_confirm()

    async def async_step_reauth_confirm(
        self, user_input: dict[str, Any] | None = None
    ) -> ConfigFlowResult:
        errors: dict[str, str] = {}
        entry = self._get_reauth_entry()
        if user_input is not None:
            try:
                info = await self._async_server_info({**entry.data, **user_input})
            except InvalidAuth:
                errors["base"] = "invalid_auth"
            except CannotConnect:
                errors["base"] = "cannot_connect"
            else:
                await self.async_set_unique_id(info["server_id"])
                self._abort_if_unique_id_mismatch()
                return self.async_update_reload_and_abort(entry, data_updates=user_input)

        return self.async_show_form(
            step_id="reauth_confirm",
            data_schema=vol.Schema({vol.Required(CONF_TOKEN): str}),
            errors=errors,
        )

    async def async_step_integration_discovery(
        self, discovery_info: dict[str, Any]
    ) -> ConfigFlowResult:
        await self.async_set_unique_id(discovery_info["device_id"])
        # Aborts if the user chose "Ignore" for this device before.
        self._abort_if_unique_id_configured()
        self._device = discovery_info
        self.context["title_placeholders"] = {"name": discovery_info["name"]}
        return await self.async_step_adopt()

    async def async_step_adopt(self, user_input: dict[str, Any] | None = None) -> ConfigFlowResult:
        if user_input is not None:
            entry = self.hass.config_entries.async_get_entry(self._device["entry_id"])
            if entry is None or entry.state is not ConfigEntryState.LOADED:
                return self.async_abort(reason="server_unavailable")
            try:
                await entry.runtime_data.async_adopt(self._device["device_id"])
            except AlosaError:
                return self.async_abort(reason="adopt_failed")
            return self.async_abort(reason="device_adopted")

        return self.async_show_form(
            step_id="adopt",
            description_placeholders={
                "name": self._device["name"],
                "model": self._device["model"],
                "fingerprint": self._device["fingerprint"],
            },
        )
