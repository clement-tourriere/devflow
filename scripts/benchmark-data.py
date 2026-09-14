#!/usr/bin/env python3
"""Benchmark disposable PostgreSQL workspaces; never uses a configured project.

Example: python3 scripts/benchmark-data.py --binary ./target/debug/devflow \
  --sizes-mib 1 16 64 --concurrency 1 4 --output benchmark.json

Requires Docker and a devflow binary with service-local. Results distinguish
logical/allocated storage growth from physical bytes copied (not observable
portably for reflinks). TCP source availability is sampled, not SQL latency.
"""
import argparse
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import socket
import subprocess
import tempfile
import threading
import time


def footprint(root):
    logical = allocated = 0
    for path in root.rglob('*'):
        if path.is_file():
            stat = path.stat()
            logical += stat.st_size
            allocated += getattr(stat, 'st_blocks', 0) * 512
    return logical, allocated


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--sizes-mib', nargs='+', type=int, default=[1, 16, 64])
    parser.add_argument('--concurrency', nargs='+', type=int, default=[1, 4])
    parser.add_argument('--output', type=Path, default=Path('benchmark.json'))
    args = parser.parse_args()
    if min(args.sizes_mib + args.concurrency) <= 0:
        parser.error('sizes and concurrency must be positive')
    binary = str(args.binary.resolve())
    binary_sha256 = hashlib.sha256(Path(binary).read_bytes()).hexdigest()
    results = []
    # Use the repository filesystem so Docker Desktop can mount this directory.
    with tempfile.TemporaryDirectory(prefix='.devflow-benchmark-', dir=Path.cwd()) as directory:
        root = Path(directory)
        repo = root / 'repo'
        repo.mkdir()
        data = root / 'data'
        env = dict(os.environ, DEVFLOW_CONFIG_DIR=str(root / 'config'), DEVFLOW_PROCESS_STATE_DIR=str(root / 'processes'))
        for command in [('init', '-b', 'main'), ('config', 'user.name', 'Benchmark'), ('config', 'user.email', 'benchmark@example.invalid')]:
            subprocess.run(['git', *command], cwd=repo, check=True, capture_output=True)
        config = {'name': 'benchmark', 'project_id': root.name, 'worktree': {'path_template': '../worktrees/{workspace}', 'copy_files': []}, 'services': [{'name': 'db', 'type': 'local', 'service_type': 'postgres', 'default': True, 'auto_workspace': True, 'local': {'data_root': str(data), 'image': 'postgres:17'}}]}
        (repo / '.devflow.yml').write_text(json.dumps(config))
        subprocess.run(['git', 'add', '.'], cwd=repo, check=True, capture_output=True)
        subprocess.run(['git', '-c', 'commit.gpgsign=false', 'commit', '-m', 'fixture'], cwd=repo, check=True, capture_output=True)

        def run(*command):
            timer = ['/usr/bin/time', '-l' if platform.system() == 'Darwin' else '-v']
            started = time.monotonic()
            proc = subprocess.run(timer + [binary, '--json', '--non-interactive', *command], cwd=repo, env=env, capture_output=True, text=True)
            if proc.returncode:
                raise RuntimeError(f'{command}: {proc.stdout}\n{proc.stderr}')
            result = json.loads(proc.stdout)
            patterns = [r'(\d+)\s+maximum resident set size', r'Maximum resident set size \(kbytes\):\s*(\d+)']
            match = next((m for pattern in patterns if (m := re.search(pattern, proc.stderr))), None)
            rss = int(match.group(1)) * (1 if platform.system() == 'Darwin' else 1024) if match else None
            return result, {'elapsed_seconds': time.monotonic() - started, 'cli_peak_rss_bytes': rss}

        try:
            run('switch', 'main', '--no-verify', '--no-processes')
            for size in args.sizes_mib:
                fixture = root / 'seed.sql'
                fixture.write_text(f"DROP TABLE IF EXISTS benchmark_data; CREATE TABLE benchmark_data AS SELECT n, repeat(md5(n::text),32) AS payload FROM generate_series(1,{size * 1024}) n;\n")
                run('service', 'seed', 'main', '--from', str(fixture))
                baseline, preparation = run('baseline', 'prepare', f'data{size}', '--service', 'db', '--from', str(fixture))
                connection, _ = run('service', 'connection', 'main')
                for count in args.concurrency:
                    for origin in ['main', baseline['reference']]:
                        names = [f'bench/{size}-{count}-{len(results)}-{i}' for i in range(count)]
                        before = footprint(data)
                        stop = threading.Event()
                        samples = []
                        def probe():
                            while not stop.is_set():
                                now = time.monotonic()
                                try:
                                    with socket.create_connection((connection['host'], connection['port']), timeout=0.1):
                                        available = True
                                except OSError:
                                    available = False
                                samples.append((now, available))
                                stop.wait(0.1)
                        observer = threading.Thread(target=probe)
                        observer.start()
                        started = time.monotonic()
                        try:
                            with concurrent.futures.ThreadPoolExecutor(max_workers=count) as workers:
                                runs = list(workers.map(lambda name: run('switch', '-c', name, '--from', 'main', '--data-from', origin, '--no-verify', '--no-processes'), names))
                            elapsed = time.monotonic() - started
                            resets = [run('service', 'reset', name)[1] for name in names]
                        finally:
                            stop.set()
                            observer.join()
                        after = footprint(data)
                        downtime = sum(b[0]-a[0] for a, b in zip(samples, samples[1:]) if not a[1])
                        results.append({'dataset_mib': size, 'concurrency': count, 'data_origin': origin, 'prepare': preparation, 'clone_group_seconds': elapsed, 'clones': [m for _, m in runs], 'resets': resets, 'logical_bytes_added': after[0]-before[0], 'allocated_bytes_added': after[1]-before[1], 'physical_bytes_copied': None, 'source_tcp_unavailable_seconds': downtime, 'probe_interval_seconds': 0.1})
                        args.output.write_text(json.dumps({'schema_version': 1, 'platform': platform.platform(), 'binary_sha256': binary_sha256, 'measurements': results}, indent=2) + '\n')
                        for name in names:
                            run('remove', name, '--force')
                run('baseline', 'remove', baseline['reference'])
        finally:
            # Exact fixture configuration scopes destruction to these resources.
            run('service', 'destroy', '--force')
    print(f'Wrote {len(results)} measurements to {args.output}')


if __name__ == '__main__':
    main()
