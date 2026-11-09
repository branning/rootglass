#!/usr/bin/env python3
"""Boot the signed example through the installed macOS app's Ratatui TUI."""

from __future__ import annotations

import fcntl
import glob
import os
import pty
import re
import select
import shutil
import struct
import subprocess
import sys
import termios
import time
from pathlib import Path


ANSI = re.compile(
    rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\))"
)
FAILURE_MARKERS = (
    b"Failed",
    b"Blocked",
    b"QEMU could not be started",
    b"QEMU exited before the guest became ready",
)
FIXTURE_PASSWORD = b"rootglass-ci"
BOOT_TIMEOUT = 120
LOGIN_TIMEOUT = 15
COMMAND_TIMEOUT = 25
STOP_TIMEOUT = 20
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
        raise SmokeFailure(
            "install the pyte terminal emulator before running the smoke"
        ) from error

    screen = pyte.Screen(TERMINAL_COLUMNS, TERMINAL_ROWS)
    pyte.Stream(screen).feed(output.decode("utf-8", errors="replace"))
    return screen


def terminal_screen_text(output: bytes) -> str:
    return "\n".join(terminal_screen(output).display)


def save_terminal_screenshot(output: bytes, destination: Path) -> None:
    try:
        from PIL import Image, ImageDraw, ImageFont
    except ImportError as error:
        raise SmokeFailure("install Pillow before creating the TUI screenshot") from error

    screen = terminal_screen(output)
    font_paths = (
        "/System/Library/Fonts/Menlo.ttc",
        "/System/Library/Fonts/Supplemental/Courier New.ttf",
        "DejaVuSansMono.ttf",
    )
    font = None
    for font_path in font_paths:
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
        if len(value) == 6 and all(
            character in "0123456789abcdefABCDEF" for character in value
        ):
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


def read_serial(directory: Path) -> bytes:
    output = bytearray()
    for path in serial_logs(directory):
        try:
            output.extend(path.read_bytes())
        except OSError:
            continue
    return bytes(output)


def qemu_processes(binary: Path) -> list[tuple[int, str]]:
    result = subprocess.run(
        ["ps", "-axo", "pid=,command="],
        check=True,
        capture_output=True,
        text=True,
    )
    matches = []
    for line in result.stdout.splitlines():
        fields = line.strip().split(maxsplit=1)
        if len(fields) != 2 or str(binary) not in fields[1]:
            continue
        try:
            pid = int(fields[0])
        except ValueError:
            continue
        if pid != os.getpid():
            matches.append((pid, fields[1]))
    return matches


class TuiSession:
    def __init__(self, binary: Path, glass: Path, output: Path):
        self.output = output
        self.terminal_log = output / "tui-terminal-raw.bin"
        self.capture = bytearray()
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 120, 0, 0))

        environment = os.environ.copy()
        environment["TERM"] = "xterm-256color"
        environment["LC_ALL"] = "C"
        environment["ROOTGLASS_TUI_TRACE_FILE"] = str(output / "tui-trace.log")
        self.child = subprocess.Popen(
            [str(binary), "tui", str(glass)],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=environment,
            close_fds=True,
        )
        os.close(slave)
        os.set_blocking(self.master, False)

    def drain_terminal(self) -> None:
        while True:
            try:
                ready, _, _ = select.select([self.master], [], [], 0)
                if not ready:
                    return
                chunk = os.read(self.master, 65536)
            except OSError:
                return
            if not chunk:
                return
            self.capture.extend(chunk)

    def send(self, data: bytes) -> None:
        os.write(self.master, data)

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

    def send_console_line(self, line: bytes, timeout: int = 10) -> None:
        after = self.current_screen()
        self.send(b"\r")
        self.wait_for_terminal(b"Guest input:", timeout, after)
        self.send(line + b"\r")

    def wait_for_serial(self, pattern: bytes, timeout: int) -> bytes:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.drain_terminal()
            screen = clean_terminal(self.capture)
            self.check_for_startup_failure(screen)
            self.check_child()
            output = read_serial(Path(os.environ["TMPDIR"]))
            if re.search(pattern, clean_terminal(output)):
                return output
            time.sleep(0.05)
        raise SmokeFailure(f"timed out waiting for serial output matching: {pattern!r}")

    @staticmethod
    def check_for_startup_failure(screen: bytes) -> None:
        for marker in FAILURE_MARKERS:
            if marker in screen:
                raise SmokeFailure(f"TUI reported QEMU startup failure: {marker.decode()}")

    def check_child(self) -> None:
        status = self.child.poll()
        if status is not None:
            raise SmokeFailure(f"packaged Rootglass TUI exited early with status {status}")

    def save_evidence(self) -> None:
        self.drain_terminal()
        self.terminal_log.write_bytes(self.capture)
        (self.output / "tui-terminal.txt").write_bytes(clean_terminal(self.capture))
        screenshot = self.output / "tui-terminal-screenshot.png"
        if not screenshot.exists():
            save_terminal_screenshot(bytes(self.capture), screenshot)
        for index, path in enumerate(serial_logs(Path(os.environ["TMPDIR"]))):
            shutil.copy2(path, self.output / f"qemu-{index}.log")

    def save_screenshot(self) -> None:
        self.drain_terminal()
        save_terminal_screenshot(
            bytes(self.capture), self.output / "tui-terminal-screenshot.png"
        )

    def quit_cleanly(self) -> None:
        if self.child.poll() is None:
            # Ctrl-Q is handled globally by the TUI, including its console mode.
            self.send(b"\x11")
            try:
                self.child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.child.terminate()
                try:
                    self.child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.child.kill()
                    self.child.wait()
        self.drain_terminal()
        self.terminal_log.write_bytes(self.capture)
        (self.output / "tui-terminal.txt").write_bytes(clean_terminal(self.capture))
        os.close(self.master)


def run(binary: Path, glass: Path, output: Path) -> None:
    binary = binary.resolve()
    glass = glass.resolve()
    output.mkdir(parents=True, exist_ok=True)
    log_directory = Path(os.environ.get("TMPDIR", "/tmp"))
    log_directory.mkdir(parents=True, exist_ok=True)
    for old_log in serial_logs(log_directory):
        old_log.unlink(missing_ok=True)

    session = TuiSession(binary, glass, output)
    runtime_root = binary.parents[1] / "Resources" / "rootglass" / "runtime-assets"
    qemu_binaries = list(runtime_root.glob("*/bin/qemu-system-aarch64"))
    if len(qemu_binaries) != 1:
        session.quit_cleanly()
        raise SmokeFailure(
            f"expected one bundled QEMU under {runtime_root}, found {len(qemu_binaries)}"
        )
    qemu_binary = qemu_binaries[0]
    try:
        session.wait_for_terminal(b"Rootglass Examples", 60)
        after = session.current_screen()
        session.send(b"p")
        session.wait_for_terminal(b"Verified launch plan", 30, after)
        plan_output = session.current_screen().encode("utf-8")
        if not re.search(rb"TCG\s*\(software\s*emulation\)", plan_output):
            raise SmokeFailure("macOS runner did not select the expected TCG fallback")
        normalized_plan_output = re.sub(rb"\s+", b"", plan_output).lower()
        if b"-accelhvf" in normalized_plan_output:
            raise SmokeFailure("launch plan selected HVF on a runner that cannot use it")
        after = session.current_screen()
        session.send(b"b")

        session.wait_for_serial(rb"rootglass-hello login:\s*$", BOOT_TIMEOUT)
        session.wait_for_terminal(b"rootglass-hello login:", 60, after)
        session.wait_for_terminal(b"Running", 30, after)
        session.send_console_line(b"root")
        session.wait_for_serial(rb"(?i)password:\s*$", LOGIN_TIMEOUT)
        session.wait_for_terminal(b"Password:", 10)
        session.send_console_line(FIXTURE_PASSWORD)
        session.wait_for_serial(rb"(?m)#\s*$", LOGIN_TIMEOUT)

        command = (
            b"printf 'ROOTGLASS_TUI_SMOKE_BEGIN\\n'; uname -a; "
            b"printf 'ROOTGLASS_TUI_SMOKE_DONE\\n'"
        )
        session.send_console_line(command)
        session.wait_for_serial(
            rb"ROOTGLASS_TUI_SMOKE_DONE(?:\r\n|\n|\r)", COMMAND_TIMEOUT
        )
        session.wait_for_terminal(b"ROOTGLASS_TUI_SMOKE_DONE", COMMAND_TIMEOUT)
        serial = read_serial(log_directory).replace(b"\r", b"")
        lines = [line.strip() for line in serial.splitlines()]
        try:
            begin = lines.index(b"ROOTGLASS_TUI_SMOKE_BEGIN")
            done = lines.index(b"ROOTGLASS_TUI_SMOKE_DONE", begin + 1)
        except ValueError as error:
            raise SmokeFailure("guest command completion markers were not emitted") from error
        uname = next(
            (line for line in lines[begin + 1 : done] if line.startswith(b"Linux rootglass-hello")),
            None,
        )
        if uname is None or b"aarch64" not in uname:
            raise SmokeFailure("guest uname output did not identify the expected aarch64 guest")

        session.save_screenshot()
        session.send(b"s")
        deadline = time.monotonic() + STOP_TIMEOUT
        while time.monotonic() < deadline and qemu_processes(qemu_binary):
            session.drain_terminal()
            time.sleep(0.1)
        remaining = qemu_processes(qemu_binary)
        if remaining:
            raise SmokeFailure(f"QEMU remained after TUI stop: {remaining}")

        session.send(b"q")
        status = session.child.wait(timeout=10)
        if status != 0:
            raise SmokeFailure(f"packaged Rootglass TUI exited with status {status}")
        if qemu_processes(qemu_binary):
            raise SmokeFailure("a bundled QEMU process remained after Rootglass exited")

        (output / "guest-uname.txt").write_bytes(uname + b"\n")
        session.save_evidence()
        print(f"TUI booted the example and reported: {uname.decode(errors='replace')}")
        print("TUI stop completed and no bundled QEMU process remained.")
    except Exception:
        session.save_evidence()
        raise
    finally:
        session.quit_cleanly()


def main() -> int:
    if len(sys.argv) != 4:
        print(
            "usage: smoke-rootglass-installed-macos-tui.py ROOTGLASS_BINARY EXAMPLE_GLASS OUTPUT_DIR",
            file=sys.stderr,
        )
        return 2
    try:
        run(Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3]))
    except Exception as error:
        print(f"macOS installed TUI smoke failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
