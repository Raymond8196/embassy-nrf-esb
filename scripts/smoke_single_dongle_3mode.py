#!/usr/bin/env python3
"""Single-dongle smoke test for the converged PRX 3-mode example.

Builds and flashes `mpsl_3mode_central`, captures USB CDC logs, and checks the
no-peer invariants that are meaningful with one dongle.
"""

from __future__ import annotations

import argparse
import glob
import re
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import serial
import serial.tools.list_ports


ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target/thumbv7em-none-eabihf/release/examples/mpsl_3mode_central"


def run(cmd: list[str]) -> None:
    print("+", " ".join(cmd), flush=True)
    subprocess.run(cmd, cwd=ROOT, check=True)


def matching_ports(descriptions: tuple[str, ...]):
    ports = list(serial.tools.list_ports.comports())
    for port in ports:
        desc = port.description or ""
        if any(needle in desc for needle in descriptions):
            yield port


def find_port(preferred: str | None, descriptions: tuple[str, ...]) -> str:
    if preferred:
        return preferred
    ports = list(serial.tools.list_ports.comports())
    for port in matching_ports(descriptions):
        return port.device
    acm_ports = sorted(glob.glob("/dev/ttyACM*"))
    if len(acm_ports) == 1:
        return acm_ports[0]
    known = ", ".join(f"{p.device}({p.description})" for p in ports) or "none"
    raise RuntimeError(f"no matching serial port found; saw: {known}")


def find_serial_number(preferred: str | None, descriptions: tuple[str, ...]) -> str | None:
    if preferred:
        return preferred
    for port in matching_ports(descriptions):
        if port.serial_number:
            return port.serial_number
    return None


def wait_for_port(descriptions: tuple[str, ...], timeout_s: float) -> str:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        try:
            return find_port(None, descriptions)
        except RuntimeError:
            time.sleep(0.25)
    return find_port(None, descriptions)


def build_package(app_version: int) -> Path:
    run(
        [
            "cargo",
            "build",
            "--example",
            "mpsl_3mode_central",
            "--features",
            "nrf52840,defmt,mpsl",
            "--release",
        ]
    )
    tmp = Path(tempfile.mkdtemp(prefix="esb-3mode-smoke-"))
    hex_path = tmp / "mpsl_3mode_central.hex"
    zip_path = tmp / "mpsl_3mode_central.zip"
    run(["arm-none-eabi-objcopy", "-O", "ihex", str(BIN), str(hex_path)])
    run(
        [
            "python3",
            "-m",
            "nordicsemi",
            "pkg",
            "generate",
            "--application",
            str(hex_path),
            "--hw-version",
            "52",
            "--sd-req",
            "0x00",
            "--application-version",
            str(app_version),
            str(zip_path),
        ]
    )
    return zip_path


def flash(zip_path: Path, port: str | None, serial_number: str | None) -> None:
    cmd = [
        "nrfutil",
        "dfu",
        "usb-serial",
        "-pkg",
        str(zip_path),
        "-b",
        "115200",
        "-t",
        "30",
    ]
    if serial_number:
        print(f"Using DFU serial number: {serial_number}", flush=True)
        cmd += ["-snr", serial_number]
    elif port:
        print(f"Using DFU port: {port}", flush=True)
        cmd += ["-p", port]
    else:
        raise RuntimeError("no DFU port or serial number available")
    run(cmd)


def flash_with_retries(
    zip_path: Path, preferred_port: str | None, preferred_serial: str | None, retries: int
) -> None:
    last_error: Exception | None = None
    for attempt in range(1, retries + 1):
        if not preferred_port and not preferred_serial:
            wait_for_port(("Open DFU Bootloader", "Central"), 30.0)
        serial_number = find_serial_number(preferred_serial, ("Open DFU Bootloader", "Central"))
        port = preferred_port if not serial_number else None
        if not port and not serial_number:
            port = wait_for_port(("Open DFU Bootloader", "Central"), 30.0)
        try:
            print(f"DFU attempt {attempt}/{retries}", flush=True)
            flash(zip_path, port, serial_number)
            return
        except Exception as exc:
            last_error = exc
            print(f"DFU attempt {attempt} failed: {exc}", file=sys.stderr, flush=True)
            time.sleep(1.0)
    assert last_error is not None
    raise last_error


def capture(port: str, seconds: float) -> str:
    last_stderr = ""
    for _ in range(3):
        deadline = time.monotonic() + 10.0
        while not os.path.exists(port) and time.monotonic() < deadline:
            time.sleep(0.25)
        result = subprocess.run(
            ["timeout", f"{seconds}s", "cat", port],
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if result.returncode in (0, 124):
            sys.stdout.buffer.write(result.stdout)
            sys.stdout.buffer.flush()
            return result.stdout.decode("utf-8", errors="replace")
        last_stderr = result.stderr.decode("utf-8", errors="replace").strip()
        time.sleep(1.0)
    raise RuntimeError(last_stderr)


def assert_smoke(log: str) -> None:
    if "[3MODE] PRX converged engine active" not in log:
        raise AssertionError("missing converged PRX activation log")
    reports = [line for line in log.splitlines() if line.startswith("b=")]
    if not reports:
        raise AssertionError("no PRX report lines captured")

    parsed = []
    for line in reports:
        fields = dict(re.findall(r"([a-z0-9]+)=([0-9]+)", line))
        parsed.append((line, fields))

    if not any(int(fields.get("s", "0")) > 0 and int(fields.get("t0", "0")) > 0 for _, fields in parsed):
        raise AssertionError("no report had both s>0 and t0>0")

    for line, fields in parsed:
        for key in ("iv", "ov", "dt", "sc"):
            if int(fields.get(key, "0")) != 0:
                raise AssertionError(f"{key} was non-zero in report: {line}")
        if int(fields.get("rx", "0")) != 0 or int(fields.get("rd", "0")) != 0:
            raise AssertionError(f"no-peer smoke expected rx=0 and rd=0: {line}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", help="DFU bootloader port, e.g. /dev/ttyACM0")
    parser.add_argument("--serial-number", help="DFU USB serial number")
    parser.add_argument("--app-port", help="CDC app port after flashing")
    parser.add_argument("--capture-seconds", type=float, default=12.0)
    parser.add_argument("--app-version", type=int, default=2)
    parser.add_argument("--flash-retries", type=int, default=3)
    parser.add_argument("--skip-flash", action="store_true")
    args = parser.parse_args()

    try:
        if not args.skip_flash:
            package = build_package(args.app_version)
            flash_with_retries(package, args.port, args.serial_number, args.flash_retries)
        app_port = args.app_port or wait_for_port(("Central",), 10.0)
        log = capture(app_port, args.capture_seconds)
        assert_smoke(log)
    except Exception as exc:
        print(f"\nFAIL: {exc}", file=sys.stderr)
        return 1

    print("\nPASS: single-dongle 3mode smoke invariants held")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
