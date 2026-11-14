"""Read-only checks that this laptop can act as a hands-free unit. Changes nothing."""

import asyncio
import shutil
import subprocess
from pathlib import Path

from dbus_fast import BusType, Variant
from dbus_fast.aio import MessageBus
from dbus_fast.errors import DBusError

from . import msbc
from .bluez import BLUEZ, HFP_AG_UUID, HFP_HF_UUID

SPA_BLUEZ_DIRS = (
    Path("/usr/lib/x86_64-linux-gnu/spa-0.2/bluez5"),
    Path("/usr/lib/spa-0.2/bluez5"),
)


def _kernel_lines() -> list[str]:
    if shutil.which("journalctl") is None:
        return []
    try:
        out = subprocess.run(
            ["journalctl", "-k", "-b", "--no-pager", "-o", "cat"],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        ).stdout
    except OSError, subprocess.TimeoutExpired:
        return []
    keys = ("Synchronous Connection", "HW/SW Version", "SCO socket")
    return [
        line for line in out.splitlines() if "Bluetooth" in line and any(k in line for k in keys)
    ]


async def _bluez(adapter: str) -> list[str]:
    lines: list[str] = []
    bus = await MessageBus(bus_type=BusType.SYSTEM).connect()
    try:
        root = await bus.introspect(BLUEZ, "/")
        manager = bus.get_proxy_object(BLUEZ, "/", root).get_interface(
            "org.freedesktop.DBus.ObjectManager"
        )
        objects = await manager.call_get_managed_objects()
    finally:
        bus.disconnect()
    path = f"/org/bluez/{adapter}"
    props = objects.get(path, {}).get("org.bluez.Adapter1")
    if props is None:
        return [f"FAIL  adapter {adapter} not found in BlueZ"]

    def value(d: dict[str, Variant], key: str, default: object = None) -> object:
        v = d.get(key)
        return v.value if v is not None else default

    uuids = value(props, "UUIDs", [])
    lines.append(
        f"ok    adapter {adapter} {value(props, 'Address')} powered={value(props, 'Powered')}"
    )
    lines.append(
        f"info  pairable={value(props, 'Pairable')} discoverable={value(props, 'Discoverable')}"
    )
    if HFP_HF_UUID in uuids:
        lines.append(
            "WARN  a hands-free (HFP HF) profile is already registered on this adapter: "
            "another program (PipeWire bluez5, oFono or a running daemon) owns it"
        )
    else:
        lines.append("ok    no other hands-free profile is registered")
    phones = []
    for dev_path, interfaces in objects.items():
        device = interfaces.get("org.bluez.Device1")
        if not dev_path.startswith(path + "/") or device is None:
            continue
        if HFP_AG_UUID in value(device, "UUIDs", []):
            phones.append(
                f"{value(device, 'Alias')} paired={value(device, 'Paired')} "
                f"trusted={value(device, 'Trusted')} connected={value(device, 'Connected')}"
            )
    if phones:
        lines += [f"ok    phone (audio gateway): {p}" for p in phones]
    else:
        lines.append("info  no paired phone offers the audio gateway profile yet")
    return lines


def run(adapter: str) -> int:
    report: list[str] = []
    try:
        report += asyncio.run(_bluez(adapter))
    except (DBusError, OSError) as exc:
        report.append(f"FAIL  cannot reach bluetoothd on the system bus: {exc}")
    if any(d.is_dir() for d in SPA_BLUEZ_DIRS):
        report.append(
            "WARN  PipeWire's Bluetooth plugin is installed; it may claim the hands-free profile"
        )
    else:
        report.append("ok    PipeWire Bluetooth plugin not installed (no profile conflict)")
    report.append(
        "ok    libsbc found: mSBC wideband possible"
        if msbc.available()
        else "info  libsbc not found: narrowband (CVSD) only"
    )
    report += [f"info  kernel: {line}" for line in _kernel_lines()]
    print("\n".join(report))
    return 1 if any(line.startswith("FAIL") for line in report) else 0
