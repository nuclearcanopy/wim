#!/usr/bin/env python3
"""Linux process benchmarks. See docs/performance.md for scope and caveats."""

import argparse
import fcntl
import hashlib
import json
import math
import os
import platform
import pty
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time
from pathlib import Path


def percentile(values, fraction):
    values = sorted(values)
    if not values:
        raise ValueError("no samples")
    return values[max(0, math.ceil(len(values) * fraction) - 1)]


def proc_snapshot(root):
    """Return summed RSS and CPU ticks of currently live descendants on Linux."""
    processes = {}
    for path in Path('/proc').iterdir():
        if not path.name.isdigit():
            continue
        try:
            # comm is parenthesized and may contain spaces or parentheses.
            stat = (path / 'stat').read_text().rsplit(')', 1)[1].split()
            processes[int(path.name)] = (int(stat[1]), int(stat[11]) + int(stat[12]),
                                         int(stat[21]) * os.sysconf('SC_PAGE_SIZE'))
        except (OSError, ValueError, IndexError):
            continue
    if root not in processes:
        raise RuntimeError('benchmark process exited before measurement')
    descendants = {root}
    while True:
        expanded = descendants | {pid for pid, (parent, _, _) in processes.items()
                                  if parent in descendants}
        if expanded == descendants:
            break
        descendants = expanded
    return (sum(processes[p][2] for p in descendants),
            sum(processes[p][1] for p in descendants), len(descendants))


def run_once(binary, seconds, timeout):
    with tempfile.TemporaryDirectory(prefix='wim-bench-') as tmp:
        root = Path(tmp)
        for name in ('config/wim', 'cache', 'state', 'data', 'music'):
            (root / name).mkdir(parents=True)
        # An explicit, empty library avoids scanning the user's real files.
        (root / 'config/wim/config.toml').write_text(
            'library_paths = [' + json.dumps(str(root / 'music')) + ']\n')
        env = os.environ.copy()
        env.update(HOME=tmp, XDG_CONFIG_HOME=str(root / 'config'),
                   XDG_CACHE_HOME=str(root / 'cache'), XDG_STATE_HOME=str(root / 'state'),
                   XDG_DATA_HOME=str(root / 'data'), TERM='xterm-256color')
        for key in ('WIM_DIAGNOSTICS_FILE', 'WIM_COLOR', 'MPV_HOME'):
            env.pop(key, None)
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 120, 0, 0))
        start = time.monotonic()
        process = subprocess.Popen([str(binary)], stdin=slave, stdout=slave, stderr=slave,
                                   env=env, start_new_session=True)
        os.close(slave)
        try:
            ready, _, _ = select.select([master], [], [], timeout)
            if not ready:
                raise RuntimeError('no terminal output before timeout')
            data = os.read(master, 65536)
            if not data or process.poll() is not None:
                raise RuntimeError('process exited before terminal output: ' + repr(data[-500:]))
            first_output_ms = (time.monotonic() - start) * 1000
            # Let initialization settle; drain the PTY so terminal writes cannot block.
            deadline = time.monotonic() + 2.0
            while time.monotonic() < deadline:
                if select.select([master], [], [], min(0.1, max(0, deadline-time.monotonic())))[0]:
                    os.read(master, 65536)
                if process.poll() is not None:
                    raise RuntimeError('process exited during startup')
            rss, ticks, count = proc_snapshot(process.pid)
            rss_samples = [rss]
            max_processes = count
            current_ticks = ticks
            began = time.monotonic()
            deadline = began + seconds
            while time.monotonic() < deadline:
                if select.select([master], [], [], min(0.1, max(0, deadline-time.monotonic())))[0]:
                    os.read(master, 65536)
                current_rss, current_ticks, count = proc_snapshot(process.pid)
                rss_samples.append(current_rss)
                max_processes = max(max_processes, count)
            elapsed = time.monotonic() - began
            return {'first_terminal_output_ms': first_output_ms,
                    'idle_rss_mib': percentile(rss_samples, .5) / (1024 * 1024),
                    'idle_cpu_percent_one_core': max(0, current_ticks - ticks) /
                    os.sysconf('SC_CLK_TCK') / elapsed * 100,
                    'max_processes': max_processes}
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


def command_output(args):
    try:
        return subprocess.check_output(args, stderr=subprocess.DEVNULL, text=True).strip()
    except (OSError, subprocess.CalledProcessError):
        return 'unavailable'


def render(result):
    rows = ['| Metric | Median | p95 |', '| --- | ---: | ---: |']
    for key, label, unit in [
        ('first_terminal_output_ms', 'First terminal output', 'ms'),
        ('idle_rss_mib', 'Idle process-tree RSS', 'MiB'),
        ('idle_cpu_percent_one_core', 'Idle process-tree CPU', '% of one core'),
    ]:
        vals = [sample[key] for sample in result['samples']]
        rows.append(f'| {label} | {percentile(vals, .5):.2f} {unit} | '
                    f'{percentile(vals, .95):.2f} {unit} |')
    return '\n'.join(rows)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    run = sub.add_parser('run')
    run.add_argument('--binary', type=Path, required=True)
    run.add_argument('--runs', type=int, default=10)
    run.add_argument('--seconds', type=float, default=5)
    run.add_argument('--timeout', type=float, default=15)
    run.add_argument('--output', type=Path, required=True)
    report = sub.add_parser('report')
    report.add_argument('results', type=Path)
    args = parser.parse_args()
    if args.command == 'report':
        print(render(json.loads(args.results.read_text())))
        return
    if args.runs < 1 or args.seconds <= 0 or args.timeout <= 0:
        parser.error('runs, seconds and timeout must be positive')
    binary = args.binary.resolve(strict=True)
    cpu = 'unavailable'
    for line in Path('/proc/cpuinfo').read_text().splitlines():
        if line.startswith('model name'):
            cpu = line.split(':', 1)[1].strip()
            break
    repo = Path(__file__).resolve().parent.parent
    git = ['git', '-C', str(repo)]
    status = command_output(git + ['status', '--porcelain'])
    result = {'schema': 1, 'scenario': 'empty local library, no playback',
              'metadata': {'commit': command_output(git + ['rev-parse', 'HEAD']),
                           'git_dirty': None if status == 'unavailable' else bool(status),
                           'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                           'binary_version': command_output([str(binary), '--version']),
                           'mpv_version': command_output(['mpv', '--version']).splitlines()[0],
                           'kernel': platform.platform(), 'cpu': cpu,
                           'runs': args.runs, 'measurement_seconds': args.seconds,
                           'terminal': '120x40 xterm-256color PTY'},
              'samples': []}
    for index in range(args.runs):
        print(f'run {index + 1}/{args.runs}', flush=True)
        result['samples'].append(run_once(binary, args.seconds, args.timeout))
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    print(render(result))


if __name__ == '__main__':
    main()
