#!/usr/bin/env python3
"""Compare client process trees under the same, operator-verified workload."""

import argparse
import hashlib
import json
import math
import os
import platform
import random
import signal
import struct
import subprocess
import sys
import time
import wave
from pathlib import Path

from bench import command_output, percentile, proc_snapshot


CLIENTS = ('wim', 'feishin', 'supersonic')


def generate_track(path, seconds=180):
    """Generate a deterministic, uncompressed stereo PCM fixture, no codecs needed."""
    rate = 44100
    # Exactly one second of a 440 Hz tone, repeated to avoid accumulating error.
    second = b''.join(struct.pack('<hh', n, n) for n in
                      (round(4000 * math.sin(2 * math.pi * 440 * i / rate))
                       for i in range(rate)))
    with wave.open(str(path), 'wb') as out:
        out.setnchannels(2)
        out.setsampwidth(2)
        out.setframerate(rate)
        for _ in range(seconds):
            out.writeframesraw(second)


def measure(pid, seconds, interval=.1):
    rss, first_ticks, processes = proc_snapshot(pid)
    samples = [rss]
    max_processes = processes
    last_ticks = first_ticks
    start = time.monotonic()
    deadline = start + seconds
    while time.monotonic() < deadline:
        time.sleep(min(interval, max(0, deadline - time.monotonic())))
        rss, last_ticks, processes = proc_snapshot(pid)
        samples.append(rss)
        max_processes = max(max_processes, processes)
    elapsed = time.monotonic() - start
    return {'rss_mib': percentile(samples, .5) / 1048576,
            'cpu_percent_one_core': max(0, last_ticks - first_ticks) /
            os.sysconf('SC_CLK_TCK') / elapsed * 100,
            'max_processes': max_processes}


def stop(process):
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()


def profile_env(workspace, client):
    home = workspace / 'profiles' / client
    home.mkdir(parents=True, exist_ok=True, mode=0o700)
    env = os.environ.copy()
    env['HOME'] = str(home)
    for key, suffix in [('XDG_CONFIG_HOME', '.config'), ('XDG_CACHE_HOME', '.cache'),
                        ('XDG_STATE_HOME', '.local/state'), ('XDG_DATA_HOME', '.local/share')]:
        env[key] = str(home / suffix)
    for key in ('MPV_HOME', 'WIM_DIAGNOSTICS_FILE'):
        env.pop(key, None)
    return env


def commands(args):
    return {'wim': [args.terminal, '-e', args.wim],
            'feishin': [args.feishin], 'supersonic': [args.supersonic]}


def render(result):
    rows = ['| Client | Median RSS | p95 RSS | Median CPU | p95 CPU |',
            '| --- | ---: | ---: | ---: | ---: |']
    for client in CLIENTS:
        values = [s for s in result['samples'] if s['client'] == client]
        if not values:
            continue
        rss = [s['rss_mib'] for s in values]
        cpu = [s['cpu_percent_one_core'] for s in values]
        rows.append(f'| {client} | {percentile(rss, .5):.1f} MiB | '
                    f'{percentile(rss, .95):.1f} MiB | '
                    f'{percentile(cpu, .5):.1f}% | {percentile(cpu, .95):.1f}% |')
    return '\n'.join(rows)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subs = parser.add_subparsers(dest='command', required=True)
    init = subs.add_parser('init', help='create generated audio fixture and private profiles')
    init.add_argument('workspace', type=Path)
    run = subs.add_parser('run', help='run operator-confirmed GUI measurements')
    run.add_argument('workspace', type=Path)
    run.add_argument('--wim', required=True)
    run.add_argument('--feishin', default='feishin')
    run.add_argument('--supersonic', default='supersonic')
    run.add_argument('--terminal', default='xterm')
    run.add_argument('--scenario', choices=['idle', 'playback'], required=True)
    run.add_argument('--server-version', required=True)
    run.add_argument('--feishin-version', required=True)
    run.add_argument('--supersonic-version', required=True)
    run.add_argument('--runs', type=int, default=5)
    run.add_argument('--seconds', type=float, default=10)
    run.add_argument('--seed', type=int, default=42)
    run.add_argument('--output', type=Path, required=True)
    report = subs.add_parser('report')
    report.add_argument('results', type=Path)
    args = parser.parse_args()
    if args.command == 'report':
        print(render(json.loads(args.results.read_text())))
        return
    workspace = args.workspace.resolve()
    if args.command == 'init':
        workspace.mkdir(parents=True, exist_ok=True, mode=0o700)
        music = workspace / 'music'
        music.mkdir(exist_ok=True)
        track = music / 'wim-benchmark-tone.wav'
        if track.exists():
            parser.error(f'refusing to overwrite {track}')
        generate_track(track)
        for client in CLIENTS:
            profile_env(workspace, client)
        print(f'Generated {track}; private client profiles are under {workspace / "profiles"}')
        return
    if args.runs < 1 or args.seconds <= 0 or args.seconds >= 150:
        parser.error('runs must be positive and seconds must be between 0 and 150')
    track = workspace / 'music' / 'wim-benchmark-tone.wav'
    if not track.is_file():
        parser.error('run init first')
    if not sys.stdin.isatty():
        parser.error('interactive operator confirmation requires a terminal')
    programs = commands(args)
    repo = Path(__file__).resolve().parent.parent
    git = ['git', '-C', str(repo)]
    status = command_output(git + ['status', '--porcelain'])
    cpu = next((line.split(':', 1)[1].strip() for line in
                Path('/proc/cpuinfo').read_text().splitlines()
                if line.startswith('model name')), 'unavailable')
    result = {'schema': 1, 'scenario': args.scenario,
              'metadata': {'fixture_sha256': hashlib.sha256(track.read_bytes()).hexdigest(),
                           'fixture': '180s 44100Hz stereo PCM 440Hz tone',
                           'operator_verified': True,
                           'runs_per_client': args.runs, 'seconds': args.seconds,
                           'seed': args.seed, 'system': platform.platform(), 'cpu': cpu,
                           'terminal': args.terminal,
                           'wim_binary_sha256': hashlib.sha256(Path(args.wim).read_bytes()).hexdigest(),
                           'wim_commit': command_output(git + ['rev-parse', 'HEAD']),
                           'git_dirty': None if status == 'unavailable' else bool(status),
                           'versions': {'navidrome': args.server_version,
                                        'feishin': args.feishin_version,
                                        'supersonic': args.supersonic_version,
                                        'mpv': command_output(['mpv', '--version']).split('\n')[0]},
                           'wim_version': command_output([args.wim, '--version'])},
              'samples': []}
    order = list(CLIENTS) * args.runs
    random.Random(args.seed).shuffle(order)
    print('Set up each client against the same isolated Navidrome fixture before measuring.')
    print('Client profiles persist in the workspace; do not use personal credentials.')
    for client in order:
        print(f'\nLaunching {client}. Scenario: {args.scenario}.')
        process = subprocess.Popen(programs[client], env=profile_env(workspace, client),
                                   stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL, start_new_session=True)
        try:
            print('Open the fixture library. ' +
                  ('Play wim-benchmark-tone.wav from the Navidrome server.'
                   if args.scenario == 'playback' else 'Stop playback and leave the library open.'))
            answer = input('Confirm the requested state and press Enter; type q to abort: ')
            if answer.strip():
                raise SystemExit('aborted; no results written')
            if process.poll() is not None:
                raise RuntimeError(f'{client} exited before measurement')
            sample = measure(process.pid, args.seconds)
            sample['client'] = client
            result['samples'].append(sample)
            print(f"RSS {sample['rss_mib']:.1f} MiB, CPU {sample['cpu_percent_one_core']:.1f}%")
        finally:
            stop(process)
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    print('\n' + render(result))


if __name__ == '__main__':
    main()
