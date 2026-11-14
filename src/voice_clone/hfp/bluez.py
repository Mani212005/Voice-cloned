"""BlueZ side of the daemon: the HFP Hands-Free profile over D-Bus and the SCO listener.

Talks to ``bluetoothd`` on the system bus with ``org.bluez.ProfileManager1`` and
implements ``org.bluez.Profile1``. It does not use or change PipeWire, does not
restart anything and needs no root: BlueZ's default D-Bus policy lets any local
user register a profile.
"""

import asyncio
import contextlib
import logging
import signal
import socket
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from dbus_fast import BusType, Variant
from dbus_fast.aio import MessageBus
from dbus_fast.annotations import DBusDict, DBusObjectPath, DBusUnixFd
from dbus_fast.errors import DBusError
from dbus_fast.service import ServiceInterface, dbus_method

from . import msbc, sco
from .clock import Clock, SystemClock
from .connection import HfpConnection, Prompts, Settings
from .policy import Action, PolicyStore
from .store import CallStore

log = logging.getLogger(__name__)

BLUEZ = "org.bluez"
HFP_HF_UUID = "0000111e-0000-1000-8000-00805f9b34fb"
HFP_AG_UUID = "0000111f-0000-1000-8000-00805f9b34fb"
PROFILE_PATH = "/org/voiceclone/hfp_hf"
HFP_VERSION = 0x0107

# SDP SupportedFeatures for an HFP HF record (HFP 1.7 table 5.3), not the AT+BRSF bits.
SDP_FEATURE_CLI = 1 << 2
SDP_FEATURE_WIDEBAND = 1 << 5

CLEANUP_INTERVAL = 3600.0


@dataclass
class DaemonConfig:
    settings: Settings
    policy_path: Path | None
    calls_dir: Path
    adapter: str = "hci0"
    observe: bool = False


def address_from_path(device_path: str) -> str:
    """``/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF`` -> ``AA:BB:CC:DD:EE:FF``."""
    return device_path.rsplit("/", 1)[-1].removeprefix("dev_").replace("_", ":")


class HandsFreeProfile(ServiceInterface):
    def __init__(self, daemon: Daemon) -> None:
        super().__init__("org.bluez.Profile1")
        self.daemon = daemon

    @dbus_method()
    def Release(self) -> None:
        log.warning("BlueZ released the hands-free profile")

    @dbus_method()
    def NewConnection(self, device: DBusObjectPath, fd: DBusUnixFd, properties: DBusDict) -> None:
        self.daemon.new_connection(device, fd)

    @dbus_method()
    def RequestDisconnection(self, device: DBusObjectPath) -> None:
        self.daemon.drop_connection(address_from_path(device))


class Daemon:
    def __init__(self, config: DaemonConfig, clock: Clock | None = None) -> None:
        self.config = config
        self.clock = clock or SystemClock()
        self.store = CallStore(config.calls_dir)
        self.policy_store = PolicyStore(config.policy_path)
        self.prompts = Prompts(config.settings.intro_wav, config.settings.beep_wav)
        self.connections: dict[str, HfpConnection] = {}
        self.bus: MessageBus | None = None
        self.adapter_address = ""
        self.sco_listener: socket.socket | None = None
        self._stop = asyncio.Event()
        self._loop: asyncio.AbstractEventLoop | None = None

    # ----- policy ---------------------------------------------------------------------

    def decide(self, number: str | None) -> Action:
        if self.config.observe:
            return Action.RING
        return self.policy_store.decide(number)

    # ----- lifecycle ----------------------------------------------------------------

    async def run(self) -> None:
        self._loop = asyncio.get_running_loop()
        for sig in (signal.SIGINT, signal.SIGTERM):
            self._loop.add_signal_handler(sig, self._stop.set)
        if self.config.settings.wideband and not msbc.available():
            log.warning("libsbc.so.1 not found: wideband disabled, using CVSD only")
            self.config.settings.wideband = False

        self.store.claim()
        self.store.cleanup(self.clock.wall())
        self.bus = await MessageBus(bus_type=BusType.SYSTEM, negotiate_unix_fd=True).connect()
        try:
            self.adapter_address = await self._adapter_address()
            self.bus.export(PROFILE_PATH, HandsFreeProfile(self))
            await self._register_profile()
            self.sco_listener = sco.listen(self.adapter_address)
            self._loop.add_reader(self.sco_listener.fileno(), self._on_sco_incoming)
            log.info(
                "hands-free profile registered on %s (%s); answer delay %.0f s; mode %s; "
                "calls stored in %s",
                self.config.adapter,
                self.adapter_address,
                self.config.settings.answer_delay,
                "observe only" if self.config.observe else "answer",
                self.config.calls_dir,
            )
            cleanup = asyncio.create_task(self._cleanup_loop())
            await self._stop.wait()
            cleanup.cancel()
        finally:
            await self._shutdown()

    async def _shutdown(self) -> None:
        log.info("shutting down")
        for conn in list(self.connections.values()):
            conn.close()
        if self.sco_listener is not None and self._loop is not None:
            with contextlib.suppress(ValueError, OSError):
                self._loop.remove_reader(self.sco_listener.fileno())
            self.sco_listener.close()
        if self.bus is not None:
            with contextlib.suppress(DBusError, OSError):
                manager = await self._interface("/org/bluez", "org.bluez.ProfileManager1")
                await manager.call_unregister_profile(PROFILE_PATH)
            self.bus.disconnect()

    # ----- D-Bus helpers --------------------------------------------------------------

    async def _interface(self, path: str, name: str) -> Any:
        assert self.bus is not None
        introspection = await self.bus.introspect(BLUEZ, path)
        return self.bus.get_proxy_object(BLUEZ, path, introspection).get_interface(name)

    async def _adapter_address(self) -> str:
        adapter = await self._interface(f"/org/bluez/{self.config.adapter}", "org.bluez.Adapter1")
        if not await adapter.get_powered():
            raise RuntimeError(f"Bluetooth adapter {self.config.adapter} is powered off")
        return str(await adapter.get_address())

    async def _register_profile(self) -> None:
        features = SDP_FEATURE_CLI
        if self.config.settings.wideband:
            features |= SDP_FEATURE_WIDEBAND
        manager = await self._interface("/org/bluez", "org.bluez.ProfileManager1")
        await manager.call_register_profile(
            PROFILE_PATH,
            HFP_HF_UUID,
            {
                "Name": Variant("s", "Voice clone hands-free"),
                "Version": Variant("q", HFP_VERSION),
                "Features": Variant("q", features),
                "RequireAuthentication": Variant("b", True),
            },
        )

    # ----- connections --------------------------------------------------------------

    def new_connection(self, device_path: str, fd: int) -> None:
        assert self._loop is not None
        address = address_from_path(device_path)
        rfcomm = socket.socket(fileno=fd)
        old = self.connections.pop(address, None)
        if old is not None:
            old.close()
        log.info("%s: phone connected the hands-free link", address)
        conn = HfpConnection(
            self._loop,
            address,
            rfcomm,
            self.config.settings,
            self.clock,
            self.decide,
            self.store,
            self.prompts,
            open_sco=self._open_sco,
            on_closed=self._forget,
        )
        self.connections[address] = conn
        conn.start()

    def drop_connection(self, address: str) -> None:
        conn = self.connections.pop(address, None)
        if conn is not None:
            conn.close()

    def _forget(self, conn: HfpConnection) -> None:
        if self.connections.get(conn.address) is conn:
            del self.connections[conn.address]

    def _open_sco(self, address: str, transparent: bool) -> asyncio.Future[socket.socket]:
        assert self._loop is not None
        return self._loop.create_task(sco.connect(self.adapter_address, address, transparent))

    def _on_sco_incoming(self) -> None:
        assert self.sco_listener is not None
        try:
            child, address = sco.accept(self.sco_listener)
        except BlockingIOError:
            return
        except OSError as exc:
            log.warning("SCO accept failed: %s", exc)
            return
        conn = self.connections.get(address)
        if conn is None or not conn.accepts_sco():
            log.info("%s: refusing call audio (no AI call in progress)", address)
            child.close()
            return
        try:
            sco.authorize(child, conn.wants_transparent_sco())
        except OSError as exc:
            log.warning("%s: could not accept SCO: %s", address, exc)
            child.close()
            return
        conn.attach_sco(child)

    # ----- background jobs ----------------------------------------------------------

    async def _cleanup_loop(self) -> None:
        while True:
            await asyncio.sleep(CLEANUP_INTERVAL)
            self.store.cleanup(self.clock.wall())
