"""WebSocket client for the Alosa server API.

Has no Home Assistant imports, so it can be tested on its own.
"""

from __future__ import annotations

import asyncio
from collections.abc import Callable
import itertools
import logging
from typing import Any

import aiohttp

_LOGGER = logging.getLogger(__name__)

REQUEST_TIMEOUT = 15

EventCallback = Callable[[str, dict[str, Any]], None]


class AlosaError(Exception):
    """Base error for the Alosa client."""


class CannotConnect(AlosaError):
    """The server could not be reached."""


class InvalidAuth(AlosaError):
    """The server rejected the API token."""


class CommandError(AlosaError):
    """The server returned an error for a command."""


class AlosaClient:
    """One connection to the Alosa server's JSON API."""

    def __init__(self, session: aiohttp.ClientSession, url: str, token: str) -> None:
        self._session = session
        self._url = url
        self._token = token
        self._ws: aiohttp.ClientWebSocketResponse | None = None
        self._ids = itertools.count(1)
        self._pending: dict[int, asyncio.Future[Any]] = {}
        self._subscribers: list[EventCallback] = []
        self.server_info: dict[str, Any] = {}

    @property
    def connected(self) -> bool:
        return self._ws is not None and not self._ws.closed

    async def connect(self) -> dict[str, Any]:
        """Connect and wait for the server's hello. Returns server info."""
        try:
            self._ws = await self._session.ws_connect(
                self._url,
                heartbeat=30,
                headers={"Authorization": f"Bearer {self._token}"},
            )
            first = await self._ws.receive_json(timeout=REQUEST_TIMEOUT)
        except aiohttp.WSServerHandshakeError as err:
            await self.close()
            if err.status == 401:
                raise InvalidAuth("The Alosa server rejected the API token") from err
            raise CannotConnect(f"Cannot connect to {self._url}: {err}") from err
        except (aiohttp.ClientError, TimeoutError, ValueError) as err:
            await self.close()
            raise CannotConnect(f"Cannot connect to {self._url}: {err}") from err

        if first.get("event") != "server_info":
            await self.close()
            raise CannotConnect(f"{self._url} is not an Alosa server")
        self.server_info = first.get("data", {})
        return self.server_info

    async def close(self) -> None:
        if self._ws is not None:
            await self._ws.close()
            self._ws = None

    def subscribe(self, callback: EventCallback) -> Callable[[], None]:
        """Call `callback(event, data)` for every server event."""
        self._subscribers.append(callback)
        return lambda: self._subscribers.remove(callback)

    async def call(self, command: str, **args: Any) -> Any:
        """Send a command and wait for its result."""
        if not self.connected:
            raise CannotConnect("Not connected")
        assert self._ws is not None
        request_id = next(self._ids)
        future: asyncio.Future[Any] = asyncio.get_running_loop().create_future()
        self._pending[request_id] = future
        try:
            await self._ws.send_json({"id": request_id, "command": command, "args": args})
            return await asyncio.wait_for(future, REQUEST_TIMEOUT)
        except TimeoutError as err:
            raise CommandError(f"{command} timed out") from err
        except aiohttp.ClientError as err:
            raise CannotConnect(f"{command} failed: {err}") from err
        finally:
            self._pending.pop(request_id, None)

    async def listen(self) -> None:
        """Read messages until the connection closes. Run this as a task."""
        assert self._ws is not None
        try:
            async for msg in self._ws:
                if msg.type is aiohttp.WSMsgType.TEXT:
                    self._handle(msg.json())
                elif msg.type in (aiohttp.WSMsgType.CLOSED, aiohttp.WSMsgType.ERROR):
                    break
        finally:
            self._ws = None
            for future in self._pending.values():
                if not future.done():
                    future.set_exception(CannotConnect("Connection lost"))

    def _handle(self, msg: dict[str, Any]) -> None:
        if "event" in msg:
            for callback in list(self._subscribers):
                try:
                    callback(msg["event"], msg.get("data", {}))
                except Exception:
                    _LOGGER.exception("Error in Alosa event callback")
            return

        future = self._pending.get(msg.get("id", -1))
        if future is None or future.done():
            if "error" in msg:
                _LOGGER.warning("Alosa server error: %s", msg["error"])
            return
        if "error" in msg:
            future.set_exception(CommandError(msg["error"]))
        else:
            future.set_result(msg.get("result"))
