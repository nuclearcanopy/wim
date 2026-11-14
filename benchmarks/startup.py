#!/usr/bin/env python3
"""Measure recognizable PTY output and visible input, not terminal setup bytes."""

import argparse
import fcntl
import hashlib
import json
import os
import pty
import re
import select
import shutil
import signal
import socket
import struct
import subprocess
import tempfile
import termios
import time
from pathlib import Path

from bench import percentile


ESCAPES = re.compile(rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\)|[()][0-9A-Za-z]|[=>78])")
INPUT = "☃".encode()


def visible(data):
    return ESCAPES.sub(b"", data)


def first_screen(data, frontend):
    text = visible(data)
    if frontend == "wim":
        return b"search" in text and b"help" in text
    return b"shellswim" in text and b"tab complete" in text


def run_once(binary, frontend, timeout, mpv_delay=0, mpv_barrier=False, history_rows=0,
             terminal_program="Alacritty", quit_while_blocked=False, catalog_rows=0,
             wait_catalog=False, open_help=False, mpv_exits=False):
    base = "/tmp/opencode" if Path("/tmp/opencode").is_dir() else None
    with tempfile.TemporaryDirectory(prefix="wim-startup-", dir=base) as tmp:
        root = Path(tmp)
        for name in ("config/wim", "cache", "state", "data", "music", "tmp"):
            (root / name).mkdir(parents=True)
        config = "library_paths = [" + json.dumps(str(root / "music")) + "]\n"
        server = None
        if catalog_rows:
            # A private, deliberately unresponsive local fixture: no real
            # server or user credentials are contacted during the benchmark.
            server = socket.socket()
            server.bind(("127.0.0.1", 0))
            server.listen(32)
            url = f"http://127.0.0.1:{server.getsockname()[1]}"
            config += ('\n[[navidrome_servers]]\nurl = ' + json.dumps(url) +
                       '\nusername = "fixture"\npassword = "fixture"\n')
            key = "fixture@" + url
            body = "".join(json.dumps({"k": "artist", "id": str(n),
                                        "name": "needlefish" if n == catalog_rows - 1
                                        else "fixture artist " + str(n)}) + "\n"
                           for n in range(catalog_rows))
            digest = lambda text: hashlib.md5(text.encode(), usedforsecurity=False).hexdigest()
            header = {"v": 1, "id": digest("wim-catalog-id\n" + key),
                      "sum": digest(body), "at": 0}
            catalog_dir = root / "cache/wim/catalogs"
            catalog_dir.mkdir(parents=True)
            (catalog_dir / (digest(key) + ".ndjson")).write_text(json.dumps(header) + "\n" + body)
        (root / "config/wim/config.toml").write_text(config)
        if history_rows:
            (root / "cache/wim").mkdir(exist_ok=True)
            with (root / "cache/wim/index.ndjson").open("w") as index:
                for n in range(history_rows):
                    index.write(json.dumps({"id": str(n), "play_count": 64,
                                            "play_days": list(range(64))}) + "\n")
        state_path = root / "state/wim/state.toml"
        original_state = 'now_playing_path = "/previous.flac"\nqueue_paths = ["/next.flac"]\nvolume = 42.0\n'
        if quit_while_blocked:
            state_path.parent.mkdir(parents=True)
            state_path.write_text(original_state)
        env = os.environ.copy()
        env.update(HOME=tmp, XDG_CONFIG_HOME=str(root / "config"),
                   XDG_CACHE_HOME=str(root / "cache"), XDG_STATE_HOME=str(root / "state"),
                   XDG_DATA_HOME=str(root / "data"), TMPDIR=str(root / "tmp"),
                   TERM="xterm-256color", TERM_PROGRAM=terminal_program)
        for key in ("WIM_DIAGNOSTICS_FILE", "WIM_STARTUP_TRACE", "MPV_HOME"):
            env.pop(key, None)
        if mpv_exits:
            (root / "bin").mkdir()
            dead = root / "bin/mpv"
            dead.write_text("#!/bin/sh\nexit 1\n")
            dead.chmod(0o755)
            env["PATH"] = str(root / "bin") + os.pathsep + env.get("PATH", "")
        elif mpv_delay or mpv_barrier:
            mpv = shutil.which("mpv")
            if mpv is None:
                raise RuntimeError("mpv is not on PATH")
            (root / "bin").mkdir()
            wrapper = root / "bin/mpv"
            wait = ('\n: > "$HOME/mpv-started"\n'
                    'while [ ! -e "$HOME/mpv-release" ]; do sleep 0.01; done\n'
                    if mpv_barrier else "\nsleep " + str(mpv_delay) + "\n")
            wrapper.write_text("#!/bin/sh" + wait + "exec " + json.dumps(mpv) + ' "$@"\n')
            wrapper.chmod(0o755)
            env["PATH"] = str(root / "bin") + os.pathsep + env.get("PATH", "")
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        terminal_before = termios.tcgetattr(slave)
        start = time.monotonic()
        process = subprocess.Popen([str(binary)], stdin=slave, stdout=slave, stderr=slave,
                                   env=env, start_new_session=True)
        try:
            data = bytearray()
            screen_at = None
            input_at = None
            result = None
            deadline = start + timeout
            while time.monotonic() < deadline:
                ready, _, _ = select.select([master], [], [], max(0, deadline - time.monotonic()))
                if not ready:
                    break
                try:
                    chunk = os.read(master, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                now = time.monotonic()
                data.extend(chunk)
                if screen_at is None and first_screen(data, frontend):
                    screen_at = now
                    data.clear()
                    if mpv_barrier:
                        while not (root / "mpv-started").exists() and time.monotonic() < deadline:
                            time.sleep(.005)
                        if not (root / "mpv-started").exists():
                            raise RuntimeError("mpv barrier was never reached")
                    if open_help:
                        os.write(master, b"?")
                        end = time.monotonic() + 3
                        while time.monotonic() < end and b"keybinds" not in visible(data).lower():
                            if select.select([master], [], [], .05)[0]:
                                data.extend(os.read(master, 65536))
                        return {"help_visible": b"keybinds" in visible(data).lower(),
                                "mpv_was_blocked": mpv_barrier and not (root / "mpv-release").exists()}
                    os.write(master, (b"/" if frontend == "wim" else b"") + INPUT)
                    input_at = time.monotonic()
                elif screen_at is not None and result is None and INPUT in visible(data):
                    result = {"first_screen_ms": (screen_at - start) * 1000,
                              "input_echo_ms": (now - input_at) * 1000,
                              "input_from_start_ms": (now - start) * 1000,
                              "mpv_was_blocked": mpv_barrier and not (root / "mpv-release").exists()}
                    if quit_while_blocked:
                        quit_at = time.monotonic()
                        os.write(master, b"\x03")
                        process.wait(timeout=3)
                        result.update(exit_ms=(time.monotonic() - quit_at) * 1000,
                                      exit_code=process.returncode,
                                      terminal_restored=termios.tcgetattr(slave) == terminal_before,
                                      state_preserved=state_path.read_text() == original_state)
                    if open_help:
                        data.clear()
                        os.write(master, b"\x7f?")
                        deadline_help = time.monotonic() + 3
                        while time.monotonic() < deadline_help and b"keybinds" not in visible(data).lower():
                            if select.select([master], [], [], .05)[0]:
                                data.extend(os.read(master, 65536))
                        result["help_visible"] = b"quit" in visible(data).lower()
                        result["mpv_was_blocked"] = mpv_barrier and not (root / "mpv-release").exists()
                        return result
                    if wait_catalog:
                        data.clear()
                        query = b"needle" if frontend == "wim" else b"search(needle)"
                        os.write(master, b"\x7f\x1b[200~" + query + b"\x1b[201~" +
                                 (b"\r" if frontend == "shellswim" else b""))
                        continue
                    return result
                elif result is not None and (b"needlefish" in visible(data) or
                                             (frontend == "shellswim" and b"nedlefish" in visible(data))):
                    result["catalog_match_ms"] = (now - start) * 1000
                    return result
            raise RuntimeError(f"{frontend} did not produce a screen and echo input; "
                               f"screen_seen={screen_at is not None}, exit={process.poll()}, "
                               f"visible_head={visible(data[:12000])[:1600]!r}, "
                               f"tail={bytes(data[-300:])!r}")
        finally:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            os.close(master)
            os.close(slave)
            if server is not None:
                server.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--frontend", choices=("wim", "shellswim"), required=True)
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument("--timeout", type=float, default=10)
    parser.add_argument("--mpv-delay", type=float, default=0)
    parser.add_argument("--history-rows", type=int, default=0)
    parser.add_argument("--catalog-rows", type=int, default=0)
    parser.add_argument("--terminal-program", default="Alacritty")
    parser.add_argument("--wait-catalog", action="store_true")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if (args.runs < 1 or args.timeout <= 0 or args.mpv_delay < 0 or
            args.history_rows < 0 or args.catalog_rows < 0):
        parser.error("runs and timeout must be positive; delay and row counts must be nonnegative")
    if args.wait_catalog and not args.catalog_rows:
        parser.error("--wait-catalog requires --catalog-rows")
    binary = args.binary.resolve(strict=True)
    samples = [run_once(binary, args.frontend, args.timeout, args.mpv_delay,
                        history_rows=args.history_rows, terminal_program=args.terminal_program,
                        catalog_rows=args.catalog_rows, wait_catalog=args.wait_catalog)
               for _ in range(args.runs)]
    result = {"frontend": args.frontend, "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "mpv_delay_seconds": args.mpv_delay, "history_rows": args.history_rows,
              "catalog_rows": args.catalog_rows,
              "terminal_program": args.terminal_program,
              "samples": samples}
    if args.output:
        args.output.write_text(json.dumps(result, indent=2) + "\n")
    metrics = ["first_screen_ms", "input_echo_ms", "input_from_start_ms"]
    if args.wait_catalog:
        metrics.append("catalog_match_ms")
    for key in metrics:
        values = [sample[key] for sample in samples]
        print(f"{key}: median={percentile(values, .5):.2f}ms p95={percentile(values, .95):.2f}ms")


if __name__ == "__main__":
    main()
