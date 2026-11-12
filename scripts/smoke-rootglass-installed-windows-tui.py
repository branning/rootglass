#!/usr/bin/env python3
"""Exercise an installed Windows x64 package through its Ratatui TUI."""

from __future__ import annotations

import glob
import os
import re
import select
import subprocess
import sys
import tempfile
import time
from pathlib import Path


ANSI = re.compile(rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\))")
FAILURE_MARKERS = (
    b"Failed",
    b"Blocked",
    b"QEMU could not be started",
    b"QEMU exited before the guest became ready",
)
FIXTURE_PASSWORD = b"rootglass-ci"
BOOT_TIMEOUT = 240
LOGIN_TIMEOUT = 30
COMMAND_TIMEOUT = 45
STOP_TIMEOUT = 30
TERMINAL_COLUMNS = 120
TERMINAL_ROWS = 36


class SmokeFailure(RuntimeError):
    pass


def clean_terminal(output: bytes) -> bytes:
    return ANSI.sub(b"", output).replace(b"\r", b"\n")


def terminal_screen(output: bytes):
    try:
        import pyte
    except ImportError as error:
        raise SmokeFailure("install pyte before running the Windows TUI smoke") from error

    screen = pyte.Screen(TERMINAL_COLUMNS, TERMINAL_ROWS)
    pyte.Stream(screen).feed(output.decode("utf-8", errors="replace"))
    return screen


def terminal_screen_text(output: bytes) -> str:
    # On Windows, cursor-forward updates can overwrite spaces with the TUI's
    # horizontal borders in Pyte's retained screen. Treat those cells as the
    # spaces they represent when matching prompts.
    return "\n".join(terminal_screen(output).display).replace("─", " ")


def save_terminal_screenshot(output: bytes, destination: Path) -> None:
    try:
        from PIL import Image, ImageDraw, ImageFont
    except ImportError as error:
        raise SmokeFailure("install Pillow before making the TUI screenshot") from error

    screen = terminal_screen(output)
    font = None
    for font_path in (
        r"C:\Windows\Fonts\consola.ttf",
        r"C:\Windows\Fonts\lucon.ttf",
    ):
        try:
            font = ImageFont.truetype(font_path, 16)
            break
        except OSError:
            continue
    if font is None:
        font = ImageFont.load_default(size=16)

    color_names = {
        "black": "#242933",
        "red": "#bf616a",
        "green": "#a3be8c",
        "yellow": "#ebcb8b",
        "blue": "#81a1c1",
        "magenta": "#b48ead",
        "cyan": "#88c0d0",
        "white": "#e5e9f0",
        "brightblack": "#4c566a",
        "brightred": "#bf616a",
        "brightgreen": "#a3be8c",
        "brightyellow": "#ebcb8b",
        "brightblue": "#81a1c1",
        "brightmagenta": "#b48ead",
        "brightcyan": "#8fbcbb",
        "brightwhite": "#eceff4",
    }

    def color(value: str, default: str) -> str:
        if value == "default":
            return default
        if len(value) == 6 and all(c in "0123456789abcdefABCDEF" for c in value):
            return f"#{value}"
        return color_names.get(value, default)

    bounds = font.getbbox("Mg")
    cell_width = max(1, round(font.getlength("M")))
    cell_height = max(1, bounds[3] - bounds[1] + 5)
    image = Image.new(
        "RGB",
        (TERMINAL_COLUMNS * cell_width, TERMINAL_ROWS * cell_height),
        "#10151d",
    )
    draw = ImageDraw.Draw(image)
    for row_index in range(TERMINAL_ROWS):
        for column_index in range(TERMINAL_COLUMNS):
            cell = screen.buffer[row_index][column_index]
            x = column_index * cell_width
            y = row_index * cell_height
            background = color(cell.bg, "#10151d")
            if background != "#10151d":
                draw.rectangle(
                    (x, y, x + cell_width - 1, y + cell_height - 1),
                    fill=background,
                )
            if cell.data != " ":
                draw.text(
                    (x, y - bounds[1]),
                    cell.data,
                    font=font,
                    fill=color(cell.fg, "#d8dee9"),
                    stroke_width=1 if cell.bold else 0,
                )
    image.save(destination)


def serial_logs(directory: Path) -> list[Path]:
    return sorted(
        (Path(path) for path in glob.glob(str(directory / "rootglass-qemu-*.log"))),
        key=lambda path: path.stat().st_mtime,
    )


def read_serial_paths(paths: list[Path]) -> bytes:
    output = bytearray()
    for path in paths:
        try:
            output.extend(path.read_bytes())
        except OSError:
            continue
    return bytes(output)


def qemu_processes(binary: Path) -> list[str]:
    environment = os.environ.copy()
    environment["ROOTGLASS_QEMU_PATH"] = str(binary.resolve())
    query = (
        "$target = $env:ROOTGLASS_QEMU_PATH; "
        "Get-CimInstance Win32_Process -Filter \"Name='qemu-system-aarch64.exe'\" | "
        "Where-Object { $_.ExecutablePath -and "
        "$_.ExecutablePath -ieq $target } | "
        "ForEach-Object { \"$($_.ProcessId)`t$($_.ExecutablePath)\" }"
    )
    result = subprocess.run(
        ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", query],
        check=True,
        capture_output=True,
        text=True,
        env=environment,
    )
    return [line.strip() for line in result.stdout.splitlines() if line.strip()]


class TuiSession:
    def __init__(self, binary: Path, glass: Path, output: Path):
        try:
            from winpty import PtyProcess
        except ImportError as error:
            raise SmokeFailure("install pywinpty before running the Windows TUI smoke") from error

        self.output = output
        self.terminal_log = output / "tui-terminal-raw.bin"
        self.capture = bytearray()
        environment = os.environ.copy()
        environment["TERM"] = "xterm-256color"
        environment["LC_ALL"] = "C"
        environment["ROOTGLASS_TUI_TRACE_FILE"] = str(output / "tui-trace.log")
        self.child = PtyProcess.spawn(
            [str(binary), "tui", str(glass)],
            env=environment,
            dimensions=(TERMINAL_ROWS, TERMINAL_COLUMNS),
        )

    def drain_terminal(self) -> None:
        while True:
            try:
                ready, _, _ = select.select([self.child.fileobj], [], [], 0)
                if not ready:
                    return
                chunk = self.child.fileobj.recv(65536)
            except (OSError, EOFError):
                return
            if not chunk:
                return
            self.capture.extend(chunk)

    def send(self, data: bytes) -> None:
        self.child.write(data.decode("utf-8"))

    def current_screen(self) -> str:
        self.drain_terminal()
        return terminal_screen_text(bytes(self.capture))

    def wait_for_terminal(
        self, needle: bytes, timeout: int, after: str | None = None
    ) -> None:
        deadline = time.monotonic() + timeout
        previous_length = -1
        screen = ""
        while time.monotonic() < deadline:
            self.drain_terminal()
            if len(self.capture) != previous_length:
                previous_length = len(self.capture)
                screen = terminal_screen_text(bytes(self.capture))
            text = needle.decode("utf-8", errors="replace")
            if text in screen and (after is None or screen != after):
                return
            self.check_for_startup_failure(clean_terminal(self.capture))
            self.check_child()
            time.sleep(0.05)
        visible_tail = "\n".join(screen.splitlines()[-8:])
        raise SmokeFailure(
            f"timed out waiting for TUI text: {needle.decode(errors='replace')}\n"
            f"Current terminal screen:\n{visible_tail}"
        )

    def wait_for_terminal_without(self, needle: bytes, timeout: int) -> None:
        deadline = time.monotonic() + timeout
        text = needle.decode("utf-8", errors="replace")
        screen = ""
        while time.monotonic() < deadline:
            self.drain_terminal()
            screen = terminal_screen_text(bytes(self.capture))
            if text not in screen:
                return
            self.check_for_startup_failure(clean_terminal(self.capture))
            self.check_child()
            time.sleep(0.05)
        visible_tail = "\n".join(screen.splitlines()[-8:])
        raise SmokeFailure(
            f"TUI kept {text!r} visible after Enter\nCurrent terminal screen:\n{visible_tail}"
        )

    def send_console_line(self, line: bytes, timeout: int = 15) -> None:
        after = self.current_screen()
        self.send(b"\r")
        self.wait_for_terminal(b"Guest input:", timeout, after)
        self.send(b"\x1b[200~" + line + b"\x1b[201~")
        self.wait_for_terminal(b"Guest input: " + line, timeout)
        self.send(b"\r")
        self.wait_for_terminal_without(b"Guest input:", min(timeout, 5))

    def wait_for_serial(self, pattern: bytes, timeout: int, logs: list[Path] | None = None) -> bytes:
        deadline = time.monotonic() + timeout
        log_directory = Path(tempfile.gettempdir())
        while time.monotonic() < deadline:
            self.drain_terminal()
            self.check_for_startup_failure(clean_terminal(self.capture))
            self.check_child()
            output = read_serial_paths(
                logs if logs is not None else serial_logs(log_directory)
            )
            if re.search(pattern, clean_terminal(output)):
                return output
            time.sleep(0.05)
        raise SmokeFailure(f"timed out waiting for QEMU serial output matching {pattern!r}")

    def wait_for_new_serial_log(
        self, directory: Path, previous_logs: set[Path], timeout: int
    ) -> Path:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            new_logs = [path for path in serial_logs(directory) if path not in previous_logs]
            if new_logs:
                return new_logs[-1]
            self.drain_terminal()
            self.check_for_startup_failure(clean_terminal(self.capture))
            self.check_child()
            time.sleep(0.05)
        raise SmokeFailure("restart did not create a new QEMU diagnostic log")

    @staticmethod
    def check_for_startup_failure(screen: bytes) -> None:
        for marker in FAILURE_MARKERS:
            if marker in screen:
                raise SmokeFailure(f"TUI reported startup failure: {marker.decode()}")

    def check_child(self) -> None:
        if not self.child.isalive():
            raise SmokeFailure(f"packaged Rootglass TUI exited early: {self.child.exitstatus}")

    def save_evidence(self) -> None:
        self.drain_terminal()
        self.terminal_log.write_bytes(self.capture)
        (self.output / "tui-terminal.txt").write_bytes(clean_terminal(self.capture))
        screenshot = self.output / "tui-terminal-screenshot.png"
        if not screenshot.exists():
            try:
                save_terminal_screenshot(bytes(self.capture), screenshot)
            except Exception as error:  # preserve the primary smoke failure and raw transcript
                (self.output / "screenshot-error.txt").write_text(
                    str(error), encoding="utf-8"
                )
        for index, path in enumerate(serial_logs(Path(tempfile.gettempdir()))):
            try:
                (self.output / f"qemu-{index}.log").write_bytes(path.read_bytes())
            except OSError:
                pass

    def save_screenshot(self) -> None:
        self.drain_terminal()
        save_terminal_screenshot(bytes(self.capture), self.output / "tui-terminal-screenshot.png")

    def quit_cleanly(self) -> None:
        if self.child.isalive():
            self.send(b"\x11")
            deadline = time.monotonic() + 10
            while self.child.isalive() and time.monotonic() < deadline:
                self.drain_terminal()
                time.sleep(0.05)
            if self.child.isalive():
                self.child.terminate(force=True)
        self.drain_terminal()
        self.terminal_log.write_bytes(self.capture)
        (self.output / "tui-terminal.txt").write_bytes(clean_terminal(self.capture))
        self.child.close(force=True)


def guest_uname(serial: bytes, begin_marker: bytes, done_marker: bytes) -> bytes:
    lines = [line.strip() for line in serial.replace(b"\r", b"").splitlines()]
    try:
        begin = lines.index(begin_marker)
        done = lines.index(done_marker, begin + 1)
    except ValueError as error:
        raise SmokeFailure("guest command completion markers were not emitted") from error
    uname = next(
        (line for line in lines[begin + 1 : done] if line.startswith(b"Linux rootglass-hello")),
        None,
    )
    if uname is None or b"aarch64" not in uname:
        raise SmokeFailure("guest uname output did not identify the expected aarch64 guest")
    return uname


def verify_runtime(binary: Path, output: Path) -> Path:
    runtime_root = binary.parent / "rootglass" / "runtime-assets"
    qemu_binaries = list(runtime_root.glob("*/bin/qemu-system-aarch64.exe"))
    if len(qemu_binaries) != 1:
        raise SmokeFailure(
            f"expected one bundled QEMU under {runtime_root}, found {len(qemu_binaries)}"
        )
    qemu = qemu_binaries[0]
    version_root = qemu.parent.parent
    manifest_path = version_root / "assets.json"
    info_path = version_root / "runtime-info.json"
    import hashlib
    import json

    manifest = json.loads(manifest_path.read_text(encoding="utf-8-sig"))
    info = json.loads(info_path.read_text(encoding="utf-8-sig"))
    rows = []
    for asset in manifest["assets"]:
        asset_path = version_root / asset["path"].replace("/", os.sep)
        digest = hashlib.sha256(asset_path.read_bytes()).hexdigest()
        if digest.lower() != asset["sha256"].lower():
            raise SmokeFailure(f"bundled runtime checksum mismatch: {asset_path}")
        rows.append(f"{asset['kind']} {asset['path']} {digest}")
    (output / "runtime-inventory.txt").write_text(
        "\n".join(
            [
                f"runtime version: {manifest['runtime_version']}",
                f"target platform: {info['target_platform']}",
                f"QEMU: {info['qemu_version']}",
                *rows,
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    return qemu


def run(binary: Path, glass: Path, output: Path) -> None:
    binary = binary.resolve()
    glass = glass.resolve()
    output.mkdir(parents=True, exist_ok=True)
    log_directory = Path(tempfile.gettempdir())
    log_directory.mkdir(parents=True, exist_ok=True)
    for old_log in serial_logs(log_directory):
        old_log.unlink(missing_ok=True)

    qemu_binary = verify_runtime(binary, output)
    session = TuiSession(binary, glass, output)
    try:
        session.wait_for_terminal(b"Rootglass Examples", 60)
        session.wait_for_terminal(b"Preflight", 60)
        after = session.current_screen()
        session.send(b"p")
        session.wait_for_terminal(b"Verified launch plan", 40, after)
        plan = session.current_screen()
        (output / "launch-plan.txt").write_text(plan, encoding="utf-8")
        after = session.current_screen()
        session.send(b"b")

        session.wait_for_serial(rb"rootglass-hello login:\s*$", BOOT_TIMEOUT)
        session.wait_for_terminal(b"rootglass-hello login:", BOOT_TIMEOUT, after)
        session.wait_for_terminal(b"Running", 40, after)
        session.send_console_line(b"root")
        session.wait_for_serial(rb"(?i)password:\s*$", LOGIN_TIMEOUT)
        session.wait_for_terminal(b"Password:", 20)
        session.send_console_line(FIXTURE_PASSWORD)
        session.wait_for_serial(rb"(?m)#\s*$", LOGIN_TIMEOUT)

        begin_marker = b"ROOTGLASS_TUI_SMOKE_BEGIN"
        done_marker = b"ROOTGLASS_TUI_SMOKE_DONE"
        first_logs = serial_logs(log_directory)
        session.send_console_line(b"echo " + begin_marker)
        session.wait_for_serial(
            rb"(?m)^ROOTGLASS_TUI_SMOKE_BEGIN$", COMMAND_TIMEOUT, first_logs
        )
        session.wait_for_serial(
            rb"(?ms)^ROOTGLASS_TUI_SMOKE_BEGIN$.*^rootglass-hello:~#\s*$",
            COMMAND_TIMEOUT,
            first_logs,
        )
        session.send_console_line(b"uname -a")
        session.wait_for_serial(
            rb"(?ms)^Linux rootglass-hello\b.*\baarch64\b.*^rootglass-hello:~#\s*$",
            COMMAND_TIMEOUT,
            first_logs,
        )
        session.send_console_line(b"echo " + done_marker)
        session.wait_for_serial(
            rb"(?ms)^ROOTGLASS_TUI_SMOKE_DONE$.*^rootglass-hello:~#\s*$",
            COMMAND_TIMEOUT,
            first_logs,
        )
        session.wait_for_terminal(b"ROOTGLASS_TUI_SMOKE_DONE", COMMAND_TIMEOUT)
        uname = guest_uname(
            read_serial_paths(first_logs),
            begin_marker,
            done_marker,
        )
        session.save_screenshot()

        previous_logs = set(first_logs)
        after = session.current_screen()
        session.send(b"r")
        session.wait_for_terminal(b"Restarting guest", 40, after)
        restart_log = session.wait_for_new_serial_log(
            log_directory, previous_logs, BOOT_TIMEOUT
        )
        session.wait_for_serial(rb"rootglass-hello login:\s*$", BOOT_TIMEOUT, [restart_log])
        session.wait_for_terminal(b"rootglass-hello login:", BOOT_TIMEOUT, after)
        session.wait_for_terminal(b"Running", 40, after)
        active = qemu_processes(qemu_binary)
        if len(active) != 1:
            raise SmokeFailure(f"expected one QEMU process after restart, found {active}")

        session.send_console_line(b"root")
        session.wait_for_serial(rb"(?i)password:\s*$", LOGIN_TIMEOUT, [restart_log])
        session.wait_for_terminal(b"Password:", 20)
        session.send_console_line(FIXTURE_PASSWORD)
        session.wait_for_serial(rb"(?m)#\s*$", LOGIN_TIMEOUT, [restart_log])
        restart_begin_marker = b"ROOTGLASS_TUI_RESTART_BEGIN"
        restart_done_marker = b"ROOTGLASS_TUI_RESTART_DONE"
        session.send_console_line(b"echo " + restart_begin_marker)
        session.wait_for_serial(
            rb"(?m)^ROOTGLASS_TUI_RESTART_BEGIN$", COMMAND_TIMEOUT, [restart_log]
        )
        session.send_console_line(b"uname -a")
        session.wait_for_serial(
            rb"(?m)^Linux rootglass-hello\b.*\baarch64\b",
            COMMAND_TIMEOUT,
            [restart_log],
        )
        session.send_console_line(b"echo " + restart_done_marker)
        session.wait_for_serial(
            rb"(?m)^ROOTGLASS_TUI_RESTART_DONE$", COMMAND_TIMEOUT, [restart_log]
        )
        session.wait_for_terminal(b"ROOTGLASS_TUI_RESTART_DONE", COMMAND_TIMEOUT)
        restarted_uname = guest_uname(
            read_serial_paths([restart_log]),
            restart_begin_marker,
            restart_done_marker,
        )
        if restarted_uname != uname:
            raise SmokeFailure("guest uname changed across restart")

        session.save_screenshot()
        session.send(b"s")
        deadline = time.monotonic() + STOP_TIMEOUT
        while time.monotonic() < deadline and qemu_processes(qemu_binary):
            session.drain_terminal()
            time.sleep(0.1)
        remaining = qemu_processes(qemu_binary)
        if remaining:
            raise SmokeFailure(f"QEMU remained after TUI stop: {remaining}")

        session.send(b"\x11")
        deadline = time.monotonic() + 10
        while session.child.isalive() and time.monotonic() < deadline:
            session.drain_terminal()
            time.sleep(0.05)
        if session.child.isalive():
            raise SmokeFailure("Rootglass TUI did not exit after Ctrl-Q")
        if session.child.exitstatus != 0:
            raise SmokeFailure(f"Rootglass TUI exited with {session.child.exitstatus}")
        if qemu_processes(qemu_binary):
            raise SmokeFailure("bundled QEMU remained after Rootglass exited")

        (output / "guest-uname.txt").write_bytes(
            b"initial: " + uname + b"\nrestarted: " + restarted_uname + b"\n"
        )
        session.save_evidence()
        print(f"Guest boot and restart passed: {restarted_uname.decode(errors='replace')}")
        print("Guest stop passed; no bundled QEMU process remained.")
    except Exception:
        session.save_evidence()
        raise
    finally:
        session.quit_cleanly()


def main() -> int:
    if len(sys.argv) != 4:
        print(
            "usage: smoke-rootglass-installed-windows-tui.py ROOTGLASS_EXE EXAMPLE_GLASS OUTPUT_DIR",
            file=sys.stderr,
        )
        return 2
    try:
        run(Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3]))
    except Exception as error:
        print(f"Windows installed TUI smoke failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
