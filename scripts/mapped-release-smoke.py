#!/usr/bin/env python3
"""Run only inside isolated-validation.py --root-parent /Volumes/Data.
Retains every fixture; never calls launchctl or changes real configuration.
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

root = Path(os.environ['CCS_TEST_VOLUME_FIXTURE']).resolve()
assert root.parent.name.startswith('ccs-isolated-') and root.is_relative_to(Path('/Volumes/Data'))
assert Path(os.environ['HOME']).resolve().is_relative_to(root.parent)
binary = Path(sys.argv[1]).resolve()
results = []
nested_only = '--nested-only' in sys.argv[2:]

def run(args, env, cwd=None, success=True):
    p = subprocess.run([str(a) for a in args], env=env, cwd=cwd, capture_output=True, text=True, timeout=90)
    results.append({'argv': [str(a) for a in args], 'code': p.returncode, 'stdout': p.stdout, 'stderr': p.stderr})
    (root / 'results.json').write_text(json.dumps(results, indent=2))
    assert (p.returncode == 0) == success, results[-1]
    return p.stdout

def snapshot(path):
    return {str(p.relative_to(path)): hashlib.sha256(p.read_bytes()).hexdigest() for p in path.rglob('*') if p.is_file()}

info = subprocess.run(['/usr/sbin/diskutil', 'info', '-plist', '/Volumes/Data'], capture_output=True, check=True)
import plistlib
uuid = plistlib.loads(info.stdout)['VolumeUUID']
for name_only in [False, True]:
    case = root / ('name-only' if name_only else 'full-path')
    home, config, repo, trusted = [case / n for n in ['home', 'config', 'repo', 'trusted']]
    project = trusted / '-project'
    local = home / '.claude/projects'
    for p in [config, repo, project / 'memory', local]: p.mkdir(parents=True, exist_ok=True)
    (local / '-project').symlink_to(project, target_is_directory=True)
    env = dict(os.environ, HOME=str(home), USERPROFILE=str(home), CLAUDE_CODE_SYNC_CONFIG_DIR=str(config), CLAUDE_CONFIG_DIR=str(home / '.claude'))
    original = b'{"type":"user","sessionId":"session","uuid":"u1","cwd":"/work/project","message":{"role":"user","content":"synthetic"}}\n'
    (project / 'session.jsonl').write_bytes(original)
    (project / 'memory/MEMORY.md').write_bytes(b'synthetic memory')
    if nested_only:
        (project / 'session/subagents').mkdir(parents=True)
        (project / 'session/subagents/agent-child.jsonl').write_text('{"type":"user","sessionId":"agent-child","uuid":"child","cwd":"/work/project","message":{"role":"user","content":"nested synthetic"}}\n')
    run(['git', 'init', '-b', 'main', repo], env)
    (repo / 'marker').write_text('synthetic')
    run(['git', 'add', '.'], env, repo); run(['git', 'commit', '-m', 'fixture'], env, repo)
    bare = case / 'remote.git'
    run(['git', 'init', '--bare', bare], env)
    run(['git', 'remote', 'add', 'origin', bare], env, repo)
    run(['git', 'push', '-u', 'origin', 'main'], env, repo)
    (config / 'state.json').write_text(json.dumps({'sync_repo_path':str(repo),'has_remote':True,'is_cloned_repo':False,'last_synced_commit':None}))
    base = 'use_project_name_only = ' + str(name_only).lower() + '\n[session_maintenance]\nenabled=false\n[config_sync]\nenabled=false\n'
    def mapping(target, trust, name, volume):
        return '\n[[project_roots]]\n' + '\n'.join(k+' = '+json.dumps(str(v)) for k,v in [('project_dir',name),('target',target),('trusted_root',trust),('volume_uuid',volume)])+'\n'
    valid = base + mapping(project, trusted, '-project', uuid)
    (config / 'config.toml').write_text(valid)
    run([binary, 'push', '--scheduled'], env)
    destination = repo / 'projects' / ('project' if name_only else '-project')
    assert (destination / 'memory/MEMORY.md').read_bytes() == b'synthetic memory'
    remote_session = run(['git', '--git-dir', bare, 'show', 'main:projects/' + destination.name + '/session.jsonl'], env)
    assert json.loads(remote_session)['sessionId'] == 'session'
    if nested_only:
        nested = destination / ('agent-child.jsonl' if name_only else 'session/subagents/agent-child.jsonl')
        assert nested.is_file()
        assert len(list((repo / 'projects').rglob('MEMORY.md'))) == 1
        results.append({'layout':name_only,'nested_scheduled_push':True,'one_project_memory':True})
        continue
    (destination / 'new.jsonl').write_text('{"type":"user","sessionId":"new","cwd":"/work/project","message":{"role":"user","content":"remote synthetic"}}\n')
    (destination / 'memory/MEMORY.md').write_text('remote synthetic memory')
    run(['git','add','.'],env,repo); run(['git','commit','-m','remote fixture'],env,repo)
    run(['git','push','origin','main'],env,repo)
    run([binary,'pull'],env)
    assert (project/'new.jsonl').is_file()
    assert (project/'memory/MEMORY.md').read_text() == 'remote synthetic memory'
    assert (project/'session.jsonl').read_bytes() == original
    run([binary,'session','list','--source','claude'],env)
    cache = config/'session_index.json'
    cache_before = cache.read_bytes()
    repo_before = snapshot(repo)
    child = project/'-nested'; child.mkdir(); (local/'-nested').symlink_to(child,target_is_directory=True)
    overlap = valid + mapping(child,project,'-nested',uuid)
    for invalid in [base + mapping(project,trusted,'-project','00000000-0000-0000-0000-000000000001'), overlap]:
        (config/'config.toml').write_text(invalid)
        run([binary,'push','--scheduled'],env,success=False)
        run([binary,'pull'],env,success=False)
        assert snapshot(repo) == repo_before
        # Query can report degraded scans with exit zero; preserved cache is contract.
        p=subprocess.run([str(binary),'session','list','--source','claude'],env=env,capture_output=True,text=True,timeout=90)
        results.append({'query_invalid_code':p.returncode,'stdout':p.stdout,'stderr':p.stderr})
        assert cache.read_bytes() == cache_before
    (config/'config.toml').write_text(valid)
    # Missing target: keep every original; rename only this synthetic fixture.
    project.rename(trusted/'retained-project')
    run([binary,'push','--scheduled'],env,success=False)
    assert snapshot(repo)==repo_before
    results.append({'layout':name_only,'mapped_roundtrip':True,'wrong_uuid_overlap_missing_rejected':True})
(root/'results.json').write_text(json.dumps(results,indent=2))
print(json.dumps({'passed':True,'evidence':str(root/'results.json')},indent=2))
