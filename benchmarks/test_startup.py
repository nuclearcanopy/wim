import os
import tempfile
import unittest
from pathlib import Path

import startup


class StartupBenchmarkTests(unittest.TestCase):
    def test_terminal_setup_bytes_are_not_a_screen(self):
        self.assertFalse(startup.first_screen(b"\x1b[?1049h\x1b[?2004h", "wim"))
        self.assertTrue(startup.first_screen(b"\x1b[31msearch\x1b[0m help", "wim"))
        self.assertTrue(startup.first_screen(b"shellswim \x1b[1m? commands tab complete", "shellswim"))

    def test_waits_for_a_frame_and_for_application_input_echo(self):
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "fake-wim"
            binary.write_text(
                "#!/usr/bin/env python3\n"
                "import os, time, tty\n"
                "tty.setraw(0)\n"
                "os.write(1, b'\\x1b[?1049h')\n"
                "time.sleep(.05)\n"
                "os.write(1, b'search help')\n"
                "os.write(1, os.read(0, 128))\n"
                "time.sleep(5)\n"
            )
            binary.chmod(0o755)
            result = startup.run_once(binary, "wim", 5)
            self.assertGreater(result["first_screen_ms"], 40)
            self.assertGreaterEqual(result["input_echo_ms"], 0)

    def test_real_frontends_accept_input_before_mpv_is_released(self):
        paths = [("wim", os.environ.get("WIM_STARTUP_BIN_WIM")),
                 ("shellswim", os.environ.get("WIM_STARTUP_BIN_SHELLSWIM"))]
        if not all(path for _, path in paths):
            self.skipTest("set both WIM_STARTUP_BIN_* paths to test real binaries")
        for frontend, path in paths:
            with self.subTest(frontend=frontend):
                result = startup.run_once(Path(path), frontend, 10, mpv_barrier=True)
                self.assertTrue(result["mpv_was_blocked"])

    def test_wim_does_not_lose_early_input_on_an_unrecognized_terminal(self):
        path = os.environ.get("WIM_STARTUP_BIN_WIM")
        if not path:
            self.skipTest("set WIM_STARTUP_BIN_WIM to test the real binary")
        result = startup.run_once(Path(path), "wim", 3, terminal_program="")
        self.assertGreaterEqual(result["input_echo_ms"], 0)

    def test_quitting_while_mpv_is_blocked_restores_terminal_and_keeps_state(self):
        paths = [("wim", os.environ.get("WIM_STARTUP_BIN_WIM")),
                 ("shellswim", os.environ.get("WIM_STARTUP_BIN_SHELLSWIM"))]
        if not all(path for _, path in paths):
            self.skipTest("set both WIM_STARTUP_BIN_* paths to test real binaries")
        for frontend, path in paths:
            with self.subTest(frontend=frontend):
                result = startup.run_once(Path(path), frontend, 10, mpv_barrier=True,
                                          quit_while_blocked=True)
                self.assertTrue(result["mpv_was_blocked"])
                self.assertEqual(result["exit_code"], 0)
                self.assertTrue(result["terminal_restored"])
                self.assertTrue(result["state_preserved"])

    def test_a_dead_mpv_leaves_a_responsive_app_that_exits_cleanly(self):
        paths = [("wim", os.environ.get("WIM_STARTUP_BIN_WIM")),
                 ("shellswim", os.environ.get("WIM_STARTUP_BIN_SHELLSWIM"))]
        if not all(path for _, path in paths):
            self.skipTest("set both WIM_STARTUP_BIN_* paths to test real binaries")
        for frontend, path in paths:
            with self.subTest(frontend=frontend):
                result = startup.run_once(Path(path), frontend, 10, mpv_exits=True,
                                          quit_while_blocked=True)
                self.assertEqual(result["exit_code"], 0)
                self.assertTrue(result["terminal_restored"])
                self.assertLess(result["exit_ms"], 2500)

    def test_wim_help_opens_while_mpv_is_blocked(self):
        path = os.environ.get("WIM_STARTUP_BIN_WIM")
        if not path:
            self.skipTest("set WIM_STARTUP_BIN_WIM to test the real binary")
        result = startup.run_once(Path(path), "wim", 10, mpv_barrier=True, open_help=True)
        self.assertTrue(result["mpv_was_blocked"])
        self.assertTrue(result["help_visible"])

    def test_cached_catalog_eventually_answers_an_early_search(self):
        paths = [("wim", os.environ.get("WIM_STARTUP_BIN_WIM")),
                 ("shellswim", os.environ.get("WIM_STARTUP_BIN_SHELLSWIM"))]
        if not all(path for _, path in paths):
            self.skipTest("set both WIM_STARTUP_BIN_* paths to test real binaries")
        for frontend, path in paths:
            with self.subTest(frontend=frontend):
                result = startup.run_once(Path(path), frontend, 10, catalog_rows=5000,
                                          wait_catalog=True)
                self.assertGreaterEqual(result["catalog_match_ms"], result["first_screen_ms"])


if __name__ == "__main__":
    unittest.main()
