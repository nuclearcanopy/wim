import os
import tempfile
import unittest
from pathlib import Path

import bench


class BenchmarkTests(unittest.TestCase):
    def test_nearest_rank(self):
        self.assertEqual(bench.percentile([5, 1, 4, 2, 3], .5), 3)
        self.assertEqual(bench.percentile([5, 1, 4, 2, 3], .95), 5)
        with self.assertRaises(ValueError):
            bench.percentile([], .5)

    def test_report_uses_raw_samples(self):
        samples = [{'first_terminal_output_ms': n, 'idle_rss_mib': n * 2,
                    'idle_cpu_percent_one_core': n / 10} for n in (1, 2, 3)]
        report = bench.render({'samples': samples})
        self.assertIn('| First terminal output | 2.00 ms | 3.00 ms |', report)
        self.assertIn('| Idle process-tree RSS | 4.00 MiB | 6.00 MiB |', report)

    @unittest.skipUnless(Path('/proc/self/stat').exists(), 'requires Linux /proc')
    def test_process_snapshot(self):
        rss, ticks, count = bench.proc_snapshot(os.getpid())
        self.assertGreater(rss, 0)
        self.assertGreaterEqual(ticks, 0)
        self.assertGreaterEqual(count, 1)

    @unittest.skipUnless(Path('/proc/self/stat').exists(), 'requires Linux /proc')
    def test_run_is_isolated_and_measures_child(self):
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / 'fake-wim'
            binary.write_text('#!/usr/bin/env python3\n'
                              'import os, subprocess, time\n'
                              'assert os.environ["HOME"] != ' + repr(os.environ.get('HOME')) + '\n'
                              'assert os.path.exists(os.environ["XDG_CONFIG_HOME"] + "/wim/config.toml")\n'
                              'p = subprocess.Popen(["sleep", "30"])\n'
                              'print("ready", flush=True)\n'
                              'time.sleep(30)\n')
            binary.chmod(0o755)
            result = bench.run_once(binary, .25, 5)
            self.assertGreaterEqual(result['max_processes'], 2)
            self.assertGreater(result['idle_rss_mib'], 0)
            self.assertGreaterEqual(result['idle_cpu_percent_one_core'], 0)


if __name__ == '__main__':
    unittest.main()
