#!/usr/bin/env python3
"""Boot the example and prove that a real guest shell executes a command."""

from __future__ import annotations

import os
import pty
import re
import select
import signal
import subprocess
import sys
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
GLASS = ROOT / "examples/hello-world.glass"
LOG = ROOT / ".rootglass/build/hello-world-boot.log"
MAX_LOG_BYTES = 1_000_000
BOOT_TIMEOUT = int(os.environ.get("TIMEOUT_SECONDS", "120"))
STOP_TIMEOUT = int(os.environ.get("STOP_TIMEOUT_SECONDS", "20"))
EXPECTED_ACCEL = os.environ.get("EXPECT_QEMU_ACCEL", "")
FIXTURE_PASSWORD = "rootglass-ci"


class SmokeFailure(RuntimeError):
    pass


def wait_for(fd: int, child: int, pattern: bytes, timeout: int, log: bytearray) -> None:
    expression = re.compile(pattern)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if expression.search(log):
            return
        exited, status = os.waitpid(child, os.WNOHANG)
        if exited:
            raise SmokeFailure(f"boot process exited early with status {status}")
        ready, _, _ = select.select([fd], [], [], min(0.25, deadline - time.monotonic()))
        if ready:
            try:
                data = os.read(fd, 65536)
            except OSError:
                data = b""
            if not data:
                raise SmokeFailure("guest serial console closed before expected output")
            available = MAX_LOG_BYTES - len(log)
            if available > 0:
                log.extend(data[:available])
    raise SmokeFailure(f"timed out waiting for {pattern.decode('ascii', errors='replace')}")


def drain_until_exit(fd: int, child: int, timeout: int, log: bytearray) -> int:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        exited, status = os.waitpid(child, os.WNOHANG)
        if exited:
            return os.waitstatus_to_exitcode(status)
        ready, _, _ = select.select([fd], [], [], min(0.25, deadline - time.monotonic()))
        if ready:
            try:
                data = os.read(fd, 65536)
            except OSError:
                data = b""
            available = MAX_LOG_BYTES - len(log)
            if available > 0:
                log.extend(data[:available])
    raise SmokeFailure(f"QEMU did not exit within {timeout}s after guest shutdown")


def terminate_group(child: int) -> None:
    try:
        os.killpg(child, signal.SIGTERM)
    except ProcessLookupError:
        return
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        try:
            exited, _ = os.waitpid(child, os.WNOHANG)
        except ChildProcessError:
            break
        if exited:
            return
        time.sleep(0.1)
    try:
        os.killpg(child, signal.SIGKILL)
    except ProcessLookupError:
        pass
    try:
        os.waitpid(child, 0)
    except (ChildProcessError, InterruptedError):
        pass


def main() -> int:
    LOG.parent.mkdir(parents=True, exist_ok=True)
    plan = subprocess.run(
        ["cargo", "run", "--locked", "-p", "rootglass-cli", "--", "plan", str(GLASS)],
        cwd=ROOT,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    print(plan.stdout, end="")
    if EXPECTED_ACCEL and f"-accel {EXPECTED_ACCEL}" not in plan.stdout:
        raise SmokeFailure(f"expected QEMU acceleration is missing: {EXPECTED_ACCEL}")

    child, fd = pty.fork()
    if child == 0:
        os.chdir(ROOT)
        os.execvp(
            "cargo",
            ["cargo", "run", "--locked", "-p", "rootglass-cli", "--", "boot", str(GLASS)],
        )

    log = bytearray()
    exit_code: int | None = None
    failure: Exception | None = None
    try:
        wait_for(fd, child, rb"rootglass-hello login:\s*$", BOOT_TIMEOUT, log)
        os.write(fd, b"rootglass-ci\n")
        wait_for(fd, child, rb"(?i)password:\s*$", 10, log)
        os.write(fd, (FIXTURE_PASSWORD + "\n").encode("ascii"))
        wait_for(fd, child, rb"(?m)#\s*$", 10, log)
        os.write(
            fd,
            b"printf 'ROOTGLASS_SMOKE_BEGIN\\n'; uname -a; "
            b"printf 'ROOTGLASS_SMOKE_DONE\\n'; poweroff -f\n",
        )
        wait_for(fd, child, rb"(?m)^ROOTGLASS_SMOKE_DONE\r?$", 15, log)
        if not re.search(rb"(?m)^Linux rootglass-hello .*\baarch64\b", log):
            raise SmokeFailure("guest command completed without the expected aarch64 uname output")
        exit_code = drain_until_exit(fd, child, STOP_TIMEOUT, log)
        if exit_code != 0:
            raise SmokeFailure(f"boot command exited with status {exit_code} after guest shutdown")
    except Exception as error:
        failure = error
        terminate_group(child)
    finally:
        os.close(fd)
        LOG.write_bytes(log)

    output = log.decode("utf-8", errors="replace")
    print(output, end="" if output.endswith("\n") else "\n")
    if failure:
        print(f"boot smoke failed: {failure}", file=sys.stderr)
        print(f"serial log saved to {LOG}", file=sys.stderr)
        return 1
    print(f"boot smoke succeeded; serial log saved to {LOG}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SmokeFailure as error:
        print(f"boot smoke failed: {error}", file=sys.stderr)
        raise SystemExit(1)
    except subprocess.CalledProcessError as error:
        print(error.stdout or str(error), file=sys.stderr)
        raise SystemExit(error.returncode)
