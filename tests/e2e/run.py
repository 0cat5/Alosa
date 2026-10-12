"""End-to-end tests: the real server, real agents and real sendspin-cli,
driven through the Home Assistant integration's own API client. Everything
runs on localhost: nothing is advertised on the network and no sound plays.

Run them with scripts/e2e.sh, which builds everything and sets up Python.
"""

import asyncio
from collections.abc import Callable
import math
import os
from pathlib import Path
import socket
import struct
import sys
import tempfile
from typing import Any
import wave

import aiohttp

REPO = Path(__file__).resolve().parents[2]
BIN = REPO / "target" / "debug"
sys.path.insert(0, str(REPO / "custom_components" / "alosa"))
from client import AlosaClient, CommandError, InvalidAuth  # noqa: E402

failures = 0


def check(ok: bool, what: str) -> None:
    global failures
    print(("PASS " if ok else "FAIL ") + what, flush=True)
    failures += not ok


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def audio(device: dict[str, Any]) -> dict[str, Any]:
    return (device.get("status") or {}).get("audio") or {}


class Env:
    """A scratch directory plus the processes running in it."""

    def __init__(self, tmp: Path, sendspin: Path) -> None:
        self.tmp = tmp
        self.sendspin = sendspin
        self.procs: list[asyncio.subprocess.Process] = []

    async def spawn(self, log: str, *cmd: Any) -> asyncio.subprocess.Process:
        out = open(self.tmp / f"{log}.log", "ab")
        proc = await asyncio.create_subprocess_exec(*map(str, cmd), stdout=out, stderr=out)
        self.procs.append(proc)
        return proc

    def log(self, name: str) -> str:
        return (self.tmp / f"{name}.log").read_text(errors="replace")

    async def server(self, name: str = "server") -> int:
        port = free_port()
        await self.spawn(
            name,
            BIN / "alosa-server",
            "--data-dir",
            self.tmp / name,
            "--port",
            port,
            "--mdns",
            "false",
        )
        await asyncio.sleep(0.5)
        return port

    async def agent(self, name: str, server_port: int, *extra: Any) -> asyncio.subprocess.Process:
        return await self.spawn(
            name,
            BIN / "alosa-agent",
            "--server",
            f"ws://127.0.0.1:{server_port}/device",
            "--state-dir",
            self.tmp / name,
            "--sendspin-bin",
            self.sendspin,
            "--control-socket",
            self.tmp / f"{name}.sock",
            *extra,
        )

    async def client(self, session: aiohttp.ClientSession, port: int, name: str = "server"):
        token = (self.tmp / name / "api_token").read_text().strip()
        c = AlosaClient(session, f"ws://127.0.0.1:{port}/api", token)
        await c.connect()
        asyncio.create_task(c.listen())
        return c

    async def engines(self, agent: str) -> list[str]:
        """Command lines of sendspin-cli processes belonging to an agent."""
        proc = await asyncio.create_subprocess_exec(
            "pgrep", "-a", "sendspin-cli", stdout=asyncio.subprocess.PIPE
        )
        out, _ = await proc.communicate()
        marker = str(self.tmp / agent / "sendspin")
        return [line for line in out.decode().splitlines() if marker in line]

    async def stop_all(self) -> None:
        for proc in self.procs:
            if proc.returncode is None:
                proc.terminate()
        for proc in self.procs:
            try:
                await asyncio.wait_for(proc.wait(), 10)
            except TimeoutError:
                proc.kill()


def write_tone(path: Path, seconds: int = 3) -> Path:
    """A quiet 440 Hz test tone (it only ever plays to the null output)."""
    with wave.open(str(path), "wb") as w:
        w.setnchannels(2)
        w.setsampwidth(2)
        w.setframerate(48000)
        for i in range(48000 * seconds):
            sample = int(4000 * math.sin(2 * math.pi * 440 * i / 48000))
            w.writeframesraw(struct.pack("<hh", sample, sample))
    return path


async def wait_for(
    c: AlosaClient, device_id: str, pred: Callable[[dict], bool], timeout: float = 15
) -> dict[str, Any]:
    device: dict[str, Any] = {}
    for _ in range(int(timeout * 4)):
        devices = {d["id"]: d for d in await c.call("devices/list")}
        device = devices.get(device_id, {})
        if device and pred(device):
            return device
        await asyncio.sleep(0.25)
    return device


async def test_security(env: Env, session: aiohttp.ClientSession) -> None:
    print("--- security")
    port = await env.server()
    rogue_port = await env.server("rogue")
    await env.agent("kitchen", port, "--device-id", "kitchen-1", "--profile", "eink-clock")

    try:
        await AlosaClient(session, f"ws://127.0.0.1:{port}/api", "wrong").connect()
        check(False, "wrong API token rejected")
    except InvalidAuth:
        check(True, "wrong API token rejected")

    c = await env.client(session, port)
    check("server_id" in c.server_info, "server reports its id")
    events: list[tuple[str, str | None]] = []
    c.subscribe(lambda event, data: events.append((event, data.get("id"))))

    dev = await wait_for(c, "kitchen-1", lambda d: d["online"])
    check(dev.get("online") and not dev["adopted"], "new device is pending, not adopted")
    try:
        await c.call("device/identify", device_id="kitchen-1")
        check(False, "commands to a pending device are rejected")
    except CommandError:
        check(True, "commands to a pending device are rejected")

    await c.call("device/adopt", device_id="kitchen-1")
    await asyncio.sleep(0.5)
    check((env.tmp / "kitchen" / "server.pub").exists(), "device pinned the server's key")
    check(
        await c.call("device/identify", device_id="kitchen-1") == {}, "identify works once adopted"
    )

    # Another device claiming the same id with a different key.
    imposter = await env.agent(
        "imposter", port, "--device-id", "kitchen-1", "--profile", "eink-clock"
    )
    await asyncio.sleep(2)
    imposter.terminate()
    dev = await wait_for(c, "kitchen-1", lambda d: d["online"])
    check(dev.get("online", False), "real device stays online through an impersonation attempt")
    check("presented a different key" in env.log("server"), "server rejected the imposter")

    # The adopted device, pointed at a different server, must refuse it.
    rogue = await env.spawn(
        "rogue-agent",
        BIN / "alosa-agent",
        "--server",
        f"ws://127.0.0.1:{rogue_port}/device",
        "--state-dir",
        env.tmp / "kitchen",
        "--device-id",
        "kitchen-1",
        "--profile",
        "eink-clock",
    )
    await asyncio.sleep(2)
    rogue.terminate()
    check(
        "not the server this device is paired with" in env.log("rogue-agent"),
        "paired device refused a rogue server",
    )

    stored = (env.tmp / "server" / "devices.json").read_text()
    check(
        '"online": false' in stored and '"status"' not in stored.replace('"status": null', ""),
        "device store holds no runtime state",
    )

    events.clear()
    await c.call("device/remove", device_id="kitchen-1")
    dev = await wait_for(c, "kitchen-1", lambda d: d["online"] and not d["adopted"])
    check(("device_removed", "kitchen-1") in events, "removal sends device_removed")
    check(dev.get("online") and not dev["adopted"], "removed device comes back pending")
    await c.close()
    await env.stop_all()


async def test_audio(env: Env, session: aiohttp.ClientSession) -> None:
    print("--- audio")
    port = await env.server()
    audio_port = free_port()
    agent = await env.agent(
        "living",
        port,
        "--device-id",
        "living-1",
        "--profile",
        "connect",
        "--name",
        "Living Room",
        "--audio-port",
        audio_port,
    )
    c = await env.client(session, port)
    dev = await wait_for(c, "living-1", lambda d: d["online"])
    outputs = [o["id"] for o in dev["capabilities"]["audio_out"]["outputs"]]
    check("null" in outputs, f"device reported its outputs ({len(outputs)})")
    check(
        dev["capabilities"]["audio_out"]["engine"].startswith("sendspin-cli"),
        "device reported its engine version",
    )
    check(
        audio(dev).get("engine") == "stopped" and not await env.engines("living"),
        "engine is stopped while pending",
    )

    await c.call("device/adopt", device_id="living-1")
    # Pick the silent output before anything could play.
    await c.call("device/configure", device_id="living-1", audio_output="null")
    dev = await wait_for(c, "living-1", lambda d: audio(d).get("engine") == "running")
    await asyncio.sleep(1)
    engines = await env.engines("living")
    check(len(engines) == 1 and "--output null" in engines[0], "adoption started the engine")
    check(
        bool(engines)
        and all(
            f in engines[0] for f in ("--id living-1", "--name Living Room", "--manufacturer Alosa")
        ),
        "engine runs with the device's identity",
    )

    try:
        await c.call("device/configure", device_id="living-1", audio_output="hw:99,0")
        check(False, "unknown output rejected")
    except CommandError:
        check(True, "unknown output rejected")

    stream = await env.spawn(
        "sendspin-serve",
        Path(sys.executable).with_name("sendspin"),
        "serve",
        "--port",
        free_port(),
        "--client",
        f"ws://127.0.0.1:{audio_port}/sendspin",
        write_tone(env.tmp / "tone.wav"),
    )
    dev = await wait_for(c, "living-1", lambda d: audio(d).get("playback") == "playing")
    check(audio(dev).get("playback") == "playing", "Home Assistant sees the device playing")
    stream.terminate()

    await c.call("device/rename", device_id="living-1", name="Den")
    await asyncio.sleep(2)
    engines = await env.engines("living")
    check(len(engines) == 1 and "--name Den" in engines[0], "rename reaches the engine")

    for line in await env.engines("living"):
        os.kill(int(line.split()[0]), 9)
    dev = await wait_for(c, "living-1", lambda d: audio(d).get("engine") == "failed", 5)
    check(audio(dev).get("engine") == "failed", "engine crash is reported")
    dev = await wait_for(c, "living-1", lambda d: audio(d).get("engine") == "running")
    check(audio(dev).get("engine") == "running", "crashed engine is restarted")

    await c.call("device/restart", device_id="living-1")
    await wait_for(c, "living-1", lambda d: not d["online"], 10)
    dev = await wait_for(
        c, "living-1", lambda d: d["online"] and audio(d).get("engine") == "running"
    )
    check(
        env.log("living").count("starting Alosa agent") == 2 and dev.get("online", False),
        "restart command restarts the agent",
    )
    check(agent.returncode is None, "agent restarted in place (same process)")

    agent.terminate()
    await agent.wait()
    await asyncio.sleep(0.5)
    check(not await env.engines("living"), "stopping the agent stops the engine")

    # Server down: a paired device resumes from its saved config.
    await env.stop_all()
    await env.agent(
        "living",
        port,
        "--device-id",
        "living-1",
        "--profile",
        "connect",
        "--audio-port",
        audio_port,
    )
    await asyncio.sleep(5)
    engines = await env.engines("living")
    check(
        len(engines) == 1 and "--name Den" in engines[0],
        "engine resumes from saved config while the server is down",
    )
    await env.stop_all()


async def main() -> int:
    sendspin = os.environ.get("SENDSPIN_CLI")
    if not sendspin:
        print("set SENDSPIN_CLI to a sendspin-cli binary (scripts/e2e.sh does this)")
        return 2
    with tempfile.TemporaryDirectory(prefix="alosa-e2e-") as tmp:
        # Keep sendspin-cli off mDNS so no real Music Assistant sees test players.
        wrapper = Path(tmp) / "sendspin-cli"
        wrapper.write_text(
            "#!/bin/sh\n"
            f'if [ "$1" = --config ]; then exec "{sendspin}" "$@" --no-mdns; fi\n'
            f'exec "{sendspin}" "$@"\n'
        )
        wrapper.chmod(0o755)
        async with aiohttp.ClientSession() as session:
            for test in (test_security, test_audio):
                path = Path(tmp) / test.__name__
                path.mkdir()
                env = Env(path, wrapper)
                try:
                    await test(env, session)
                except Exception as err:
                    check(False, f"{test.__name__} crashed: {err!r}")
                finally:
                    await env.stop_all()
                if failures:
                    print(f"logs in {path}:")
                    for log in sorted(path.glob("*.log")):
                        print(f"==> {log.name}\n" + log.read_text(errors="replace")[-3000:])
                    break
    print(f"\n{'FAILED' if failures else 'all passed'}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
