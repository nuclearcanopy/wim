import fcntl
import json
import os
import pty
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time
import tomllib
import unittest
from pathlib import Path

import startup


FAKE_MPV = r'''#!/usr/bin/env python3
import json, os, socket, sys, time
from pathlib import Path
root = Path(os.environ["HOME"])
sock = next(arg.split("=", 1)[1] for arg in sys.argv if arg.startswith("--input-ipc-server="))
server = socket.socket(socket.AF_UNIX)
server.bind(sock)
server.listen(1)
conn, _ = server.accept()
playlist = []
volume = 100
held = False
with conn, conn.makefile("rb") as lines:
    for line in lines:
        request = json.loads(line)
        cmd = request["command"]
        error, data = "success", None
        if (os.environ.get("MPV_HOLD_SETUP") and not held and cmd[0] == "set_property"
                and cmd[1] == "replaygain"):
            held = True
            (root / "setup-started").touch()
            while not (root / "setup-release").exists():
                time.sleep(.005)
        if cmd[0] == "loadfile":
            path, mode = cmd[1], cmd[2]
            if mode == "append-play" and not held:
                held = True
                (root / "append-started").touch()
                while not (root / "append-release").exists():
                    time.sleep(.005)
            if "rejected" in path:
                error = "error running command"
            else:
                if mode == "replace":
                    playlist.clear()
                playlist.append(path)
                (root / "playlist.json").write_text(json.dumps(playlist))
        elif cmd[0] == "set_property" and cmd[1] == "volume":
            volume = cmd[2]
            with (root / "volumes.ndjson").open("a") as log:
                log.write(json.dumps(volume) + "\n")
        elif cmd[0] == "get_property":
            data = {"volume": volume, "time-pos": 12.0}.get(cmd[1])
        reply = {"request_id": request["request_id"], "error": error, "data": data}
        conn.sendall((json.dumps(reply) + "\n").encode())
        if cmd[0] == "observe_property" and cmd[2] == "playlist-pos":
            event = {"event": "property-change", "name": "playlist-pos", "data": 0}
            conn.sendall((json.dumps(event) + "\n").encode())
        if cmd[0] == "quit":
            break
'''


class StartupIntegrationTests(unittest.TestCase):
    def test_commands_typed_during_startup_run_in_order(self):
        paths = [("wim", os.environ.get("WIM_STARTUP_BIN_WIM")),
                 ("shellswim", os.environ.get("WIM_STARTUP_BIN_SHELLSWIM"))]
        if not all(path for _, path in paths):
            self.skipTest("set both WIM_STARTUP_BIN_* paths to test real binaries")
        for frontend, binary in paths:
            with self.subTest(frontend=frontend), tempfile.TemporaryDirectory(
                    prefix="wim-startup-test-", dir="/tmp/opencode") as tmp:
                root = Path(tmp)
                for name in ("bin", "config/wim", "cache", "state", "data", "music", "tmp"):
                    (root / name).mkdir(parents=True)
                fake = root / "bin/mpv"
                fake.write_text(FAKE_MPV)
                fake.chmod(0o755)
                (root / "config/wim/config.toml").write_text(
                    "library_paths = [" + json.dumps(str(root / "music")) + "]\n")
                env = os.environ.copy()
                env.update(HOME=tmp, XDG_CONFIG_HOME=str(root / "config"),
                           XDG_CACHE_HOME=str(root / "cache"), XDG_STATE_HOME=str(root / "state"),
                           XDG_DATA_HOME=str(root / "data"), TMPDIR=str(root / "tmp"),
                           TERM="xterm-256color", TERM_PROGRAM="Alacritty", MPV_HOLD_SETUP="1",
                           PATH=str(root / "bin") + os.pathsep + env.get("PATH", ""))
                for key in ("WIM_DIAGNOSTICS_FILE", "WIM_STARTUP_TRACE", "MPV_HOME"):
                    env.pop(key, None)
                master, slave = pty.openpty()
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
                process = subprocess.Popen([binary], stdin=slave, stdout=slave, stderr=slave,
                                           env=env, start_new_session=True)
                data = bytearray()

                def wait_for(predicate, label):
                    deadline = time.monotonic() + 10
                    while time.monotonic() < deadline:
                        if predicate():
                            return
                        ready, _, _ = select.select([master], [], [], .02)
                        if ready:
                            data.extend(os.read(master, 65536))
                        if process.poll() is not None:
                            self.fail(f"{label}: process exited {process.returncode}: {bytes(data[-500:])!r}")
                    self.fail(f"timed out waiting for {label}: {bytes(data[-500:])!r}")

                try:
                    wait_for(lambda: startup.first_screen(data, frontend) and
                             (root / "setup-started").exists(), "blocked mpv setup")
                    data.clear()
                    paste = lambda text: b"\x1b[200~" + text + b"\x1b[201~\r"
                    os.write(master, (b":" if frontend == "wim" else b"") +
                             paste(b"volume(31%)") + paste(b"volume(33%)"))
                    if frontend == "shellswim":
                        wait_for(lambda: b"volume(31%)" in startup.visible(data) and
                                 b"volume(33%)" in startup.visible(data), "queued command echoes")
                    else:
                        time.sleep(.1)
                    self.assertFalse((root / "setup-release").exists())
                    data.clear()
                    (root / "setup-release").touch()
                    volume_log = root / "volumes.ndjson"
                    wait_for(lambda: volume_log.exists() and
                             len(volume_log.read_text().splitlines()) >= 2, "queued command results")
                    self.assertEqual([json.loads(line) for line in volume_log.read_text().splitlines()],
                                     [31, 33])
                    os.write(master, b"\x03")
                    process.wait(timeout=3)
                    self.assertEqual(process.returncode, 0)
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

    def test_resume_queue_remains_responsive_and_matches_mpv_after_partial_failure(self):
        binary = os.environ.get("WIM_STARTUP_BIN_WIM")
        if not binary:
            self.skipTest("set WIM_STARTUP_BIN_WIM to test the real binary")
        with tempfile.TemporaryDirectory(prefix="wim-startup-test-", dir="/tmp/opencode") as tmp:
            root = Path(tmp)
            for name in ("bin", "config/wim", "cache", "state/wim", "data", "music", "tmp"):
                (root / name).mkdir(parents=True)
            fake = root / "bin/mpv"
            fake.write_text(FAKE_MPV)
            fake.chmod(0o755)
            tracks = [root / "music" / f"{name}.flac" for name in ("first", "three", "rejected", "one")]
            for track in tracks:
                track.touch()
            (root / "config/wim/config.toml").write_text(
                "library_paths = [" + json.dumps(str(root / "music")) + "]\n")
            state = root / "state/wim/state.toml"
            state.write_text("now_playing_path = " + json.dumps(str(tracks[0])) + "\n" +
                             "queue_paths = " + json.dumps([str(path) for path in tracks[1:]]) + "\n" +
                             "position_secs = 12.0\nvolume = 50.0\n")
            env = os.environ.copy()
            env.update(HOME=tmp, XDG_CONFIG_HOME=str(root / "config"),
                       XDG_CACHE_HOME=str(root / "cache"), XDG_STATE_HOME=str(root / "state"),
                       XDG_DATA_HOME=str(root / "data"), TMPDIR=str(root / "tmp"),
                       TERM="xterm-256color", TERM_PROGRAM="Alacritty",
                       PATH=str(root / "bin") + os.pathsep + env.get("PATH", ""))
            for key in ("WIM_DIAGNOSTICS_FILE", "WIM_STARTUP_TRACE", "MPV_HOME"):
                env.pop(key, None)
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
            before = termios.tcgetattr(slave)
            process = subprocess.Popen([binary, "--resume"], stdin=slave, stdout=slave, stderr=slave,
                                       env=env, start_new_session=True)
            data = bytearray()

            def wait_for(predicate, label):
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    if predicate():
                        return
                    ready, _, _ = select.select([master], [], [], .02)
                    if ready:
                        data.extend(os.read(master, 65536))
                    if process.poll() is not None:
                        self.fail(f"{label}: process exited {process.returncode}: {bytes(data[-500:])!r}")
                self.fail(f"timed out waiting for {label}: {bytes(data[-500:])!r}")

            try:
                wait_for(lambda: startup.first_screen(data, "wim") and
                         (root / "append-started").exists(), "blocked queue restoration")
                data.clear()
                os.write(master, b"/" + startup.INPUT)
                wait_for(lambda: startup.INPUT in startup.visible(data), "input during restoration")
                self.assertFalse((root / "append-release").exists())
                (root / "append-release").touch()
                data.clear()
                wait_for(lambda: b"1 of the saved queue could not be restored" in startup.visible(data),
                         "restoration result")
                os.write(master, b"\x03")
                process.wait(timeout=3)
                self.assertEqual(process.returncode, 0)
                self.assertEqual(termios.tcgetattr(slave), before)
                saved = tomllib.loads(state.read_text())
                self.assertEqual(saved["now_playing_path"], str(tracks[0]))
                self.assertEqual(saved["queue_paths"], [str(tracks[1]), str(tracks[3])])
                self.assertEqual(json.loads((root / "playlist.json").read_text()),
                                 [str(tracks[0]), str(tracks[1]), str(tracks[3])])
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


if __name__ == "__main__":
    unittest.main()
