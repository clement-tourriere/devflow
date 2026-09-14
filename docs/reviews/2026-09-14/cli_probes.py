"""Review probes using disposable Git repositories inside this workspace."""
import json
import os
from pathlib import Path
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
BIN = HERE.parents[2] / 'target/debug/devflow'
records = []

def run(path, env, *args):
    proc = subprocess.run([str(BIN), '--json', '--non-interactive', *args], cwd=path, env=env, capture_output=True, text=True, timeout=25)
    record = {'fixture': path.name, 'args': list(args), 'exit': proc.returncode, 'stdout': proc.stdout, 'stderr': proc.stderr}
    records.append(record)
    return record

def repo(root, name, extra='', path_template='../{repo}.{workspace}'):
    path = root / name
    path.mkdir()
    env = os.environ.copy()
    env['DEVFLOW_CONFIG_DIR'] = str(root / (name + '-config'))
    env['DEVFLOW_PROCESS_STATE_DIR'] = str(root / (name + '-processes'))
    env.pop('DEVFLOW_APPROVE_HOOKS', None)
    def git(*args):
        subprocess.run(['git', '-c', 'core.hooksPath=/dev/null', '-c', 'commit.gpgsign=false', *args], cwd=path, env=env, check=True, capture_output=True)
    git('init', '-b', 'main')
    git('config', 'user.name', 'Review Fixture')
    git('config', 'user.email', 'review@example.invalid')
    config = 'name: review\nworktree:\n  path_template: ' + path_template + '\n  copy_files: []\n  copy_ignored: false\n  copy_ai_configs: false\n' + extra
    (path / '.devflow.yml').write_text(config)
    git('add', '.devflow.yml')
    git('commit', '-m', 'fixture')
    return path, env, git

with tempfile.TemporaryDirectory(prefix='fixtures-', dir=HERE) as tmp:
    root = Path(tmp)
    p, env, git = repo(root, 'shared-identity', 'services:\n  - name: primary\n    type: shared\n  - name: analytics\n    type: shared\n')
    run(p, env, '--service', 'primary', 'service', 'connection', 'main')
    run(p, env, '--service', 'analytics', 'service', 'connection', 'main')
    run(p, env, '--service', 'primary', 'service', 'connection', '_main')

    p, env, git = repo(root, 'pre-hook', 'hooks:\n  pre-switch:\n    needs-approval: "printf never-run"\n')
    run(p, env, 'switch', '-c', 'precheck', '--no-services', '--no-processes')
    run(p, env, 'link', 'main')
    run(p, env, 'switch', '-c', 'precheck', '--no-services', '--no-processes')
    run(p, env, 'switch', 'does-not-exist', '--no-services', '--no-processes')

    p, env, git = repo(root, 'nested-worktree', path_template='../nested-workspaces/{workspace}')
    run(p, env, 'link', 'main')
    r = run(p, env, 'switch', '-c', 'nested-check', '--no-services', '--no-processes')
    r['stage'] = 'missing-parent-directory'
    r['branch_remains'] = (p / '.git/refs/heads/nested-check').exists()
    r['partial_worktree_metadata'] = sorted(x.name for x in (p / '.git/worktrees').iterdir())
    (root / 'nested-workspaces').mkdir()
    r = run(p, env, 'switch', '-c', 'nested-check', '--no-services', '--no-processes')
    r['stage'] = 'retry-after-creating-parent-directory'
    git('worktree', 'prune')
    r = run(p, env, 'switch', '-c', 'nested-check', '--no-services', '--no-processes')
    r['stage'] = 'retry-after-git-worktree-prune'

    p, env, git = repo(root, 'symlink', 'hooks:\n  post-create:\n    file-action:\n      action:\n        type: write-file\n        path: escape/marker.txt\n        content: overwritten-outside-worktree\n')
    outside = root / 'outside-symlink-target'
    outside.mkdir()
    (outside / 'marker.txt').write_text('original')
    (p / 'escape').symlink_to(outside, target_is_directory=True)
    git('add', 'escape')
    git('commit', '-m', 'fixture symlink')
    run(p, env, 'link', 'main')
    r = run(p, env, 'switch', '-c', 'symlink-check', '--no-services', '--no-processes')
    r['outside_marker'] = (outside / 'marker.txt').read_text()

    p, env, git = repo(root, 'processes', 'processes:\n  provider: native\n  auto_start: true\n  daemons:\n    worker:\n      run: "sleep 20"\n      ready_cmd: "printf READY_NOISE"\n      ready_timeout: 2\n')
    env['DEVFLOW_APPROVE_HOOKS'] = '1'
    run(p, env, 'link', 'main')
    r = run(p, env, 'switch', '-c', 'first-run', '--no-services')
    r['process_record_count'] = len(list(Path(env['DEVFLOW_PROCESS_STATE_DIR']).rglob('processes/*.json')))
    try:
        run(p, env, 'process', 'start', '--all', '--workspace', 'main')
    finally:
        run(p, env, 'process', 'stop', '--all', '--workspace', 'main')
        run(p, env, 'process', 'stop', '--all', '--workspace', 'first-run')

(HERE / 'cli-results.json').write_text(json.dumps(records, indent=2) + '\n')
for r in records:
    print(json.dumps(r))
