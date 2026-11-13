import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
import wave
from pathlib import Path
from unittest import mock

import compare


class CompareTests(unittest.TestCase):
    def test_generated_audio(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'track.wav'
            compare.generate_track(path, 1)
            with wave.open(str(path), 'rb') as audio:
                self.assertEqual((audio.getnchannels(), audio.getsampwidth(), audio.getframerate()),
                                 (2, 2, 44100))
                self.assertEqual(audio.getnframes(), 44100)

    def test_profiles_are_separate_and_do_not_use_personal_home(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            profiles = [compare.profile_env(root, name) for name in compare.CLIENTS]
            self.assertEqual(len({p['HOME'] for p in profiles}), 3)
            self.assertNotIn(os.environ.get('HOME'), [p['HOME'] for p in profiles])
            for profile in profiles:
                self.assertTrue(Path(profile['HOME']).is_dir())
                self.assertEqual(Path(profile['HOME']).stat().st_mode & 0o777, 0o700)

    def test_measures_and_stops_process(self):
        process = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'],
                                   start_new_session=True)
        try:
            sample = compare.measure(process.pid, .15, .05)
            self.assertGreater(sample['rss_mib'], 0)
            self.assertGreaterEqual(sample['cpu_percent_one_core'], 0)
            self.assertEqual(sample['max_processes'], 1)
        finally:
            compare.stop(process)
        self.assertIsNotNone(process.poll())

    def test_report_groups_clients(self):
        samples = [{'client': name, 'rss_mib': i, 'cpu_percent_one_core': i / 10}
                   for name in compare.CLIENTS for i in (1, 2, 3)]
        table = compare.render({'samples': samples})
        self.assertIn('| wim | 2.0 MiB | 3.0 MiB | 0.2% | 0.3% |', table)
        self.assertIn('| feishin |', table)
        self.assertIn('| supersonic |', table)

    def test_init_refuses_to_overwrite_fixture(self):
        with tempfile.TemporaryDirectory() as tmp:
            music = Path(tmp) / 'music'
            music.mkdir()
            track = music / 'wim-benchmark-tone.wav'
            track.write_text('existing')
            result = subprocess.run([sys.executable, str(Path(compare.__file__)), 'init', tmp],
                                    capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(track.read_text(), 'existing')

    def test_full_run_writes_three_client_samples(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'music').mkdir()
            (root / 'music/wim-benchmark-tone.wav').write_bytes(b'fixture')
            fake = root / 'fake-client'
            fake.write_text('#!/usr/bin/env python3\n'
                            'import sys, time\n'
                            'if "--version" in sys.argv: print("fake 1"); sys.exit(0)\n'
                            'time.sleep(30)\n')
            fake.chmod(0o755)
            output = root / 'results.json'
            argv = ['compare.py', 'run', tmp, '--wim', str(fake), '--terminal', str(fake),
                    '--feishin', str(fake), '--supersonic', str(fake), '--scenario', 'idle',
                    '--server-version', 'test', '--feishin-version', 'test',
                    '--supersonic-version', 'test', '--runs', '1', '--seconds', '.15',
                    '--output', str(output)]
            with mock.patch.object(sys, 'argv', argv), mock.patch('sys.stdin.isatty', return_value=True), \
                    mock.patch('builtins.input', return_value=''), contextlib.redirect_stdout(io.StringIO()):
                compare.main()
            result = json.loads(output.read_text())
            self.assertEqual({s['client'] for s in result['samples']}, set(compare.CLIENTS))
            self.assertEqual(result['scenario'], 'idle')
            self.assertTrue(all(s['rss_mib'] > 0 for s in result['samples']))


@unittest.skipUnless(os.environ.get('WIM_REAL_GUI_SMOKE') == '1', 'optional real GUI smoke test')
class RealClientSmokeTests(unittest.TestCase):
    def test_clients_launch_in_isolated_profiles(self):
        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp)
            commands = {'wim': ['xterm', '-e', os.environ['WIM_BINARY']],
                        'feishin': ['feishin'], 'supersonic': ['supersonic']}
            for client, command in commands.items():
                with self.subTest(client=client):
                    process = subprocess.Popen(command, env=compare.profile_env(workspace, client),
                                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                               start_new_session=True)
                    try:
                        time.sleep(3)
                        self.assertIsNone(process.poll(), f'{client} exited during startup')
                        sample = compare.measure(process.pid, .3)
                        self.assertGreater(sample['rss_mib'], 0)
                    finally:
                        compare.stop(process)


if __name__ == '__main__':
    unittest.main()
