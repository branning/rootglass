#!/usr/bin/env python3
"""Boot the example and prove that a real guest shell executes a command."""

from __future__ import annotations

import os
import hashlib
import pty
import re
import select
import signal
import subprocess
import sys
import tempfile
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
STATE_FILE = "/state/rootglass-persistence.txt"


class SmokeFailure(RuntimeError):
    pass


def cli_command(*args: str) -> list[str]:
    binary = os.environ.get("ROOTGLASS_CLI")
    if binary:
        return [binary, *args]
    return ["cargo", "run", "--locked", "-p", "rootglass-cli", "--", *args]


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def wait_for(fd: int, child: int, pattern: bytes, timeout: int, log: bytearray) -> None:
    expression = re.compile(pattern)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if expression.search(log):
            return
        exited, status = os.waitpid(child, os.WNOHANG)
        if exited:
            # A child can exit between polling and reading its PTY. Drain any
            # final diagnostic bytes before reporting the early exit.
            while True:
                ready, _, _ = select.select([fd], [], [], 0)
                if not ready:
                    break
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    break
                if not data:
                    break
                available = MAX_LOG_BYTES - len(log)
                if available > 0:
                    log.extend(data[:available])
            detail = log.decode("utf-8", errors="replace").strip()
            suffix = f": {detail}" if detail else ""
            raise SmokeFailure(f"boot process exited early with status {os.waitstatus_to_exitcode(status)}{suffix}")
        ready, _, _ = select.select([fd], [], [], min(0.25, deadline - time.monotonic()))
        if ready:
            try:
                data = os.read(fd, 65536)
            except OSError:
                if expression.search(log):
                    return
                raise SmokeFailure("guest serial console closed before expected output")
            if not data:
                if expression.search(log):
                    return
                raise SmokeFailure("guest serial console closed before expected output")
            available = MAX_LOG_BYTES - len(log)
            if available > 0:
                log.extend(data[:available])
    detail = log.decode("utf-8", errors="replace").strip()
    suffix = f": {detail}" if detail else ""
    raise SmokeFailure(
        f"timed out waiting for {pattern.decode('ascii', errors='replace')}{suffix}"
    )


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


def run_state_session(nonce: str, write_state: bool) -> bytearray:
    child, fd = pty.fork()
    if child == 0:
        os.chdir(ROOT)
        command = cli_command("boot", str(GLASS))
        os.execvp(command[0], command)

    log = bytearray()
    try:
        wait_for(fd, child, rb"rootglass-hello login:\s*$", BOOT_TIMEOUT, log)
        wait_for(fd, child, rb"ROOTGLASS_STATE_MOUNTED /state", 10, log)
        os.write(fd, b"root\n")
        wait_for(fd, child, rb"(?i)password:\s*$", 10, log)
        os.write(fd, (FIXTURE_PASSWORD + "\n").encode("ascii"))
        wait_for(fd, child, rb"(?m)#\s*$", 10, log)

        if write_state:
            command = (
                "set -e; printf 'ROOTGLASS_SMOKE_BEGIN\\n'; uname -a; "
                f"printf '%s\\n' '{nonce}' > {STATE_FILE}; sync; "
                "printf 'ROOTGLASS_STATE_WRITE_DONE\\n'; poweroff -f\n"
            )
            sentinel = rb"ROOTGLASS_STATE_WRITE_DONE(?:\r\n|\n|\r)"
        else:
            command = (
                f"set -e; value=\"$(cat {STATE_FILE})\"; "
                f"test \"$value\" = '{nonce}'; "
                "printf 'ROOTGLASS_STATE_READ:%s\\n' \"$value\"; "
                "printf 'ROOTGLASS_STATE_READ_DONE\\n'; poweroff -f\n"
            )
            sentinel = rb"ROOTGLASS_STATE_READ_DONE(?:\r\n|\n|\r)"
        os.write(fd, command.encode("ascii"))
        wait_for(fd, child, sentinel, 20, log)
        exit_code = drain_until_exit(fd, child, STOP_TIMEOUT, log)
        if exit_code != 0:
            raise SmokeFailure(f"boot command exited with status {exit_code} after guest shutdown")
        return log
    except Exception as error:
        terminate_group(child)
        detail = log.decode("utf-8", errors="replace").strip()
        suffix = f"\nserial output:\n{detail}" if detail else ""
        raise SmokeFailure(f"{error}{suffix}") from error
    finally:
        os.close(fd)


def main() -> int:
    LOG.parent.mkdir(parents=True, exist_ok=True)
    rootfs_before = sha256(GLASS / "rootfs.img")
    template_before = sha256(GLASS / "state.img")
    log = bytearray()
    failure: Exception | None = None
    nonce = os.urandom(16).hex()
    try:
        with tempfile.TemporaryDirectory(prefix="rootglass-state-smoke-") as state_home:
            os.environ["XDG_DATA_HOME"] = state_home
            plan = subprocess.run(
                cli_command("plan", str(GLASS)),
                cwd=ROOT,
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            )
            print(plan.stdout, end="")
            if EXPECTED_ACCEL and f"-accel {EXPECTED_ACCEL}" not in plan.stdout:
                raise SmokeFailure(f"expected QEMU acceleration is missing: {EXPECTED_ACCEL}")
            write_log = run_state_session(nonce, write_state=True)
            log.extend(write_log)
            if not re.search(rb"(?m)^Linux rootglass-hello .*\baarch64\b", write_log):
                raise SmokeFailure("guest command completed without the expected aarch64 uname output")
            read_log = run_state_session(nonce, write_state=False)
            log.extend(read_log)
            expected = f"ROOTGLASS_STATE_READ:{nonce}".encode("ascii")
            if expected not in read_log:
                raise SmokeFailure("guest state did not contain the exact bytes written before restart")
            local_state = (
                Path(state_home)
                / "rootglass"
                / "states"
                / "hello-world_glass"
                / "default.img"
            )
            if not local_state.is_file():
                raise SmokeFailure(f"local state image was not created: {local_state}")
            if sha256(GLASS / "rootfs.img") != rootfs_before:
                raise SmokeFailure("the read-only rootfs changed during guest boot")
            if sha256(GLASS / "state.img") != template_before:
                raise SmokeFailure("the signed state template changed during guest boot")
    except Exception as error:
        failure = error
    finally:
        LOG.write_bytes(log)

    output = log.decode("utf-8", errors="replace")
    print(output, end="" if output.endswith("\n") else "\n")
    if failure:
        print(f"boot/state smoke failed: {failure}", file=sys.stderr)
        print(f"serial log saved to {LOG}", file=sys.stderr)
        return 1
    print(f"boot/state smoke succeeded; rootfs and state template hashes are unchanged; log saved to {LOG}")
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
