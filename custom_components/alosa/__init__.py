"""The Alosa integration: brings Alosa devices into Home Assistant."""

from __future__ import annotations

from homeassistant.config_entries import ConfigEntry
from homeassistant.const import Platform
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import ConfigEntryAuthFailed, ConfigEntryNotReady, HomeAssistantError
from homeassistant.helpers.device_registry import EVENT_DEVICE_REGISTRY_UPDATED, DeviceEntry

from .client import AlosaError, CommandError, InvalidAuth
from .const import DOMAIN
from .hub import AlosaHub

PLATFORMS = [Platform.BINARY_SENSOR, Platform.BUTTON, Platform.SELECT, Platform.SENSOR]

type AlosaConfigEntry = ConfigEntry[AlosaHub]


async def async_setup_entry(hass: HomeAssistant, entry: AlosaConfigEntry) -> bool:
    hub = AlosaHub(hass, entry)
    try:
        await hub.async_connect()
    except InvalidAuth as err:
        raise ConfigEntryAuthFailed(str(err)) from err
    except AlosaError as err:
        raise ConfigEntryNotReady(str(err)) from err

    entry.runtime_data = hub
    entry.async_on_unload(hub.async_close)
    entry.async_create_background_task(hass, hub.async_run(), "alosa_connection")
    # Renaming a device in Home Assistant renames it everywhere (e.g. Music Assistant).
    entry.async_on_unload(
        hass.bus.async_listen(EVENT_DEVICE_REGISTRY_UPDATED, hub.async_on_registry_updated)
    )

    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    return True


async def async_unload_entry(hass: HomeAssistant, entry: AlosaConfigEntry) -> bool:
    return await hass.config_entries.async_unload_platforms(entry, PLATFORMS)


async def async_remove_config_entry_device(
    hass: HomeAssistant, entry: AlosaConfigEntry, device_entry: DeviceEntry
) -> bool:
    """Let the user delete a device. The server forgets it and its pinned key."""
    hub = entry.runtime_data
    for domain, device_id in device_entry.identifiers:
        if domain != DOMAIN or device_id not in hub.devices:
            continue
        try:
            await hub.async_remove(device_id)
        except CommandError:
            # Already gone on the server side; nothing left to clean up.
            pass
        except AlosaError as err:
            raise HomeAssistantError(f"Could not remove device: {err}") from err
    return True
