#!/usr/bin/env python3
"""Synthetic scheduled push, ONLY through isolated-validation.py on Data.
Retains private fixture and raw subprocess logs. No real sessions are read.
"""
import argparse
import json
import os
from pathlib import Path
import plistlib
import subprocess
import time

parser = argparse.ArgumentParser()
parser.add_argument('binary')
parser.add_argument('--sessions-per-project', type=int, default=2)
parser.add_argument('--assert-linear', action='store_true')
args = parser.parse_args()
assert args.sessions_per_project >= 2
root = Path(os.environ['CCS_TEST_VOLUME_FIXTURE']).resolve()
assert root.parent.name.startswith('ccs-isolated-') and root.is_relative_to(Path('/Volumes/Data'))
assert Path(os.environ['HOME']).resolve().is_relative_to(root.parent)
binary = Path(args.binary).resolve()
case = root / 'perf-27'
case.mkdir()
home, config, repo, trusted = [case / name for name in ['home', 'config', 'repo', 'trusted']]
local = home / '.claude/projects'
for path in [config, repo, trusted, local]:
    path.mkdir(parents=True)
env = dict(os.environ, HOME=str(home), USERPROFILE=str(home), CLAUDE_CODE_SYNC_CONFIG_DIR=str(config), CLAUDE_CONFIG_DIR=str(home / '.claude'))

def run(argv, label, cwd=None):
    with (case / (label + '.stdout')).open('wb') as out, (case / (label + '.stderr')).open('wb') as err:
        result = subprocess.run([str(x) for x in argv], env=env, cwd=cwd, stdout=out, stderr=err, timeout=3600)
    assert result.returncode == 0, (label, result.returncode)

info = subprocess.run(['/usr/sbin/diskutil', 'info', '-plist', '/Volumes/Data'], capture_output=True, check=True)
volume_uuid = plistlib.loads(info.stdout)['VolumeUUID']
config_text = 'use_project_name_only = false\n[session_maintenance]\nenabled=false\n[config_sync]\nenabled=false\n'
for project_number in range(27):
    name = '-synthetic-%02d' % project_number
    project = trusted / name
    (project / 'memory').mkdir(parents=True)
    (local / name).symlink_to(project, target_is_directory=True)
    for session_number in range(args.sessions_per_project):
        sid = 'synthetic-%02d-%04d' % (project_number, session_number)
        entry = {'type': 'user', 'sessionId': sid, 'uuid': sid, 'cwd': '/synthetic/' + name, 'message': {'role': 'user', 'content': 'synthetic-only'}}
        (project / (sid + '.jsonl')).write_text(json.dumps(entry) + '\n')
    (project / 'memory/MEMORY.md').write_text('synthetic memory\n')
    config_text += '\n[[project_roots]]\n' + '\n'.join(key + ' = ' + json.dumps(str(value)) for key, value in [('project_dir', name), ('target', project), ('trusted_root', trusted), ('volume_uuid', volume_uuid)]) + '\n'
(config / 'config.toml').write_text(config_text)
run(['git', 'init', '-b', 'main', repo], 'init')
(repo / 'marker').write_text('synthetic')
run(['git', 'add', '.'], 'add', repo)
run(['git', 'commit', '-m', 'synthetic fixture'], 'commit', repo)
bare = case / 'remote.git'
run(['git', 'init', '--bare', bare], 'bare')
run(['git', 'remote', 'add', 'origin', bare], 'remote', repo)
run(['git', 'push', '-u', 'origin', 'main'], 'initial-push', repo)
(config / 'state.json').write_text(json.dumps({'sync_repo_path': str(repo), 'has_remote': True, 'is_cloned_repo': False, 'last_synced_commit': None}))
started = time.monotonic()
run([binary, 'push', '--scheduled'], 'scheduled-push')
elapsed = time.monotonic() - started
lines = (case / 'scheduled-push.stderr').read_text().splitlines()
perf = [line for line in lines if line.startswith('CCS_PUSH_PERF ')]
final = next(line for line in reversed(perf) if 'event=end stage=push ' in line)
counts = dict(item.split('=', 1) for item in final.split()[1:])
expected = 27 * args.sessions_per_project
assert int(counts['parsed']) == expected
for project_number in range(27):
    project = repo / 'projects' / ('-synthetic-%02d' % project_number)
    assert len(list(project.glob('*.jsonl'))) == args.sessions_per_project
    assert (project / 'memory/MEMORY.md').read_text() == 'synthetic memory\n'
if args.assert_linear:
    assert int(counts['enumerate']) == 1, counts
    assert int(counts['probe']) <= 27 * 6 + expected * 3, counts
summary = {'mappings': 27, 'sessions': expected, 'elapsed_seconds': elapsed, 'counters': counts, 'phases': perf, 'linear_asserted': args.assert_linear}
(case / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
print(json.dumps(summary, indent=2))
