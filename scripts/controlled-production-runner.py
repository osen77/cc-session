#!/usr/bin/env python3
"""Prepare a single production scheduled-push invocation; default is NO execution.

Production execution requires --execute and --authorize-production-push, an
absolute ccs path pinned by SHA256, explicit real HOME/config and an existing
artifact parent. No arbitrary argv, shell, configuration writes or CCS status
repair. Raw stdout/stderr are retained verbatim, unbounded, in a mode-0700 fresh
artifact directory: provision disk space; these files may contain sensitive CCS
output and must not be published. Only numeric process metadata is sampled;
samples are bounded and never contain argv, environment or session contents.
Any timeout or TERM-resistant group produces UNKNOWN; never escalate to SIGKILL or retry.
The separate events.jsonl/result.json are never truncated with log output.
Detached/reparented descendants are outside process-group containment; samples
are evidence, not a claim that every possible escaped process was discovered.
"""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import pwd
import signal
import subprocess
import tempfile
import time


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def save_json(path, value):
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + '\n')


def group_state(pgid):
    try:
        os.killpg(pgid, 0)
        return 'present'
    except ProcessLookupError:
        return 'absent'
    except OSError:
        return 'unknown'


def sample_group(pgid):
    # Never request comm/command/args/e: those can disclose secrets.
    try:
        sampler = subprocess.Popen(['/bin/ps', '-axo', 'pid=,ppid=,pgid=,stat=,time=,rss='],
                                   stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                   start_new_session=True)
    except OSError as error:
        return {'error': 'sampler_unavailable', 'errno': error.errno}
    try:
        data, _ = sampler.communicate(timeout=1)
    except subprocess.TimeoutExpired:
        sampler.terminate()  # Only this sampler, never a preexisting process.
        try:
            sampler.communicate(timeout=0.2)
        except subprocess.TimeoutExpired:
            pass  # Do not kill even an unresponsive sampler.
        return {'error': 'sampler_timeout', 'sampler_pid': sampler.pid}
    if sampler.returncode != 0:
        return {'error': 'sampler_failed', 'returncode': sampler.returncode}
    rows = []
    count = 0
    for line in data.decode('ascii', errors='replace').splitlines():
        fields = line.split()
        if len(fields) == 6 and fields[2] == str(pgid):
            count += 1
            if len(rows) < 64:
                rows.append(dict(zip(('pid', 'ppid', 'pgid', 'stat', 'cpu_time', 'rss_kib'), fields)))
    return {'rows': rows, 'matching_count': count, 'rows_truncated': count > len(rows)}


def run_observed(argv, environment, cwd, directory, timeout, grace, sample_interval=5,
                 max_samples=120):
    """Internal observer shared with synthetic smoke; not an arbitrary-argv CLI."""
    result = {'argv': argv, 'cwd': str(cwd), 'started_at': now(),
              'timeout_seconds': timeout, 'term_grace_seconds': grace,
              'outcome': 'unknown', 'timed_out': False, 'term_sent': False,
              'leader_returncode': None, 'residual_process_group': 'unknown',
              'sample_count': 0, 'sample_limit_reached': False,
              'raw_logs': 'verbatim, unbounded, retained; private and potentially sensitive',
              'sampling': {'interval_seconds': sample_interval, 'max_samples': max_samples,
                           'max_rows_per_sample': 64, 'scope': 'private process group only'}}
    save_json(directory / 'result.json', result)
    interrupted = []
    previous = {}
    for sig in (signal.SIGINT, signal.SIGTERM):
        previous[sig] = signal.signal(sig, lambda number, frame: interrupted.append(number))
    process = None
    group_gone = False
    with (directory / 'events.jsonl').open('a', buffering=1) as events, \
         (directory / 'stdout.log').open('wb') as stdout, \
         (directory / 'stderr.log').open('wb') as stderr:
        def event(kind, **fields):
            events.write(json.dumps({'at': now(), 'event': kind, **fields}) + '\n')
            events.flush()
        def observe_group():
            nonlocal group_gone
            if group_gone:
                return 'absent'
            state = group_state(process.pid)
            group_gone = state == 'absent'
            return state
        try:
            event('starting', argv=argv)
            process = subprocess.Popen(argv, env=environment, cwd=cwd,
                                       start_new_session=True, stdin=subprocess.DEVNULL,
                                       stdout=stdout, stderr=stderr)
            result['private_pgid'] = process.pid
            event('spawned', pid=process.pid, pgid=process.pid)
            save_json(directory / 'result.json', result)
            deadline = time.monotonic() + timeout
            next_sample = 0
            samples = 0
            while process.poll() is None:
                if interrupted:
                    result['interrupted_by'] = interrupted[0]
                    break
                if time.monotonic() >= deadline:
                    result['timed_out'] = True
                    event('timeout')
                    break
                if samples < max_samples and time.monotonic() >= next_sample:
                    event('process_sample', **sample_group(process.pid))
                    samples += 1
                    next_sample = time.monotonic() + sample_interval
                time.sleep(0.05)
            result['sample_count'] = samples
            result['sample_limit_reached'] = samples >= max_samples
        except Exception as error:
            result['observer_error'] = type(error).__name__ + ': ' + str(error)
            event('observer_error', error=result['observer_error'])
        finally:
            if process is not None:
                # Cleanup applies even if the leader exited but left its group alive.
                state = observe_group()
                event('before_cleanup', group=state, leader_returncode=process.poll())
                if state != 'absent':
                    try:
                        os.killpg(process.pid, signal.SIGTERM)
                        result['term_sent'] = True
                        event('term_sent', pgid=process.pid)
                    except ProcessLookupError:
                        group_gone = True
                        event('term_not_sent', reason='group_already_absent')
                    except OSError as error:
                        result['term_error'] = str(error)
                        event('term_failed', error=str(error))
                    grace_deadline = time.monotonic() + grace
                    while time.monotonic() < grace_deadline:
                        process.poll()
                        if observe_group() == 'absent':
                            break
                        time.sleep(0.05)
                result['leader_returncode'] = process.poll()
                result['residual_process_group'] = observe_group()
                try:
                    event('final_process_sample', **sample_group(process.pid))
                except OSError as error:
                    event('final_sample_failed', error=str(error))
                clean = result['residual_process_group'] == 'absent'
                if (clean and result['leader_returncode'] is not None
                        and not result.get('observer_error') and not result['timed_out']
                        and not interrupted and not result['term_sent']):
                    result['outcome'] = 'exited'
                # Absent processes never imply CCS success: only observed exit zero
                # without timeout/interruption/cleanup is an ordinary successful exit.
            result['finished_at'] = now()
            result['exit_code'] = (result['leader_returncode'] if result['outcome'] == 'exited'
                                   else 124 if result['timed_out'] else 125)
            if result['exit_code'] < 0:
                result['exit_code'] = 128 - result['exit_code']
            event('finished', **result)
            save_json(directory / 'result.json', result)
            for sig, handler in previous.items():
                signal.signal(sig, handler)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--ccs', required=True, type=Path)
    parser.add_argument('--binary-sha256', required=True)
    parser.add_argument('--home', required=True, type=Path)
    parser.add_argument('--config-dir', required=True, type=Path)
    parser.add_argument('--artifact-parent', required=True, type=Path)
    parser.add_argument('--timeout', type=int, default=600)
    parser.add_argument('--term-grace', type=int, default=10)
    parser.add_argument('--execute', action='store_true')
    parser.add_argument('--authorize-production-push', action='store_true',
                        help='explicit acknowledgement of one real production write operation')
    args = parser.parse_args()
    if not 1 <= args.timeout <= 1800 or not 1 <= args.term_grace <= 60:
        parser.error('timeout must be 1..1800 and term grace 1..60 seconds')
    if any(not path.is_absolute() for path in (args.ccs, args.home, args.config_dir, args.artifact_parent)):
        parser.error('all paths must be absolute')
    if args.ccs.name != 'ccs' or len(args.binary_sha256) != 64 or any(c not in '0123456789abcdef' for c in args.binary_sha256):
        parser.error('binary must be named ccs and pinned by a lowercase SHA256')
    if args.authorize_production_push and not args.execute:
        parser.error('authorization requires --execute; omit both for a dry-run')
    plan = {'mode': 'execute' if args.execute else 'dry-run',
            'argv': [str(args.ccs), 'push', '--scheduled'], 'binary_sha256': args.binary_sha256,
            'home': str(args.home), 'config_dir': str(args.config_dir),
            'artifact_parent': str(args.artifact_parent), 'timeout_seconds': args.timeout,
            'term_grace_seconds': args.term_grace, 'termination': 'new process group only; TERM only; no retry',
            'logs': 'raw stdout/stderr retained verbatim, unbounded, private; events separate',
            'sampling': '5s interval, at most 120 samples plus final, 64 rows/sample; no argv/env',
            'ccs_status_writes_by_runner': False}
    print(json.dumps(plan, indent=2), flush=True)
    if not args.execute:
        return 0  # No files created, no binary/config opened, no child launched.
    if not args.authorize_production_push:
        parser.error('execution requires --authorize-production-push; no child started')
    if os.sys.platform != 'darwin':
        parser.error('production execution is restricted to macOS')
    if any(k.startswith('CCS_TEST_') for k in os.environ):
        parser.error('production execution refuses test/isolated environments')
    real_home = Path(pwd.getpwuid(os.getuid()).pw_dir).resolve(strict=True)
    if args.home.resolve(strict=True) != real_home:
        parser.error('--home must be the current account real home, never a test HOME')
    expected_config = real_home / 'Library/Application Support/claude-code-sync'
    if args.config_dir.resolve(strict=True) != expected_config.resolve(strict=True):
        parser.error('--config-dir must be the real macOS CCS configuration directory')
    binary = args.ccs.resolve(strict=True)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error('binary must be an executable regular file')
    with binary.open('rb') as source:
        digest = hashlib.file_digest(source, 'sha256').hexdigest() if hasattr(hashlib, 'file_digest') else None
    if digest is None:
        hasher = hashlib.sha256()
        with binary.open('rb') as source:
            for block in iter(lambda: source.read(1024 * 1024), b''):
                hasher.update(block)
        digest = hasher.hexdigest()
    if digest != args.binary_sha256:
        parser.error('binary SHA256 mismatch; no child started')
    parent = args.artifact_parent.resolve(strict=True)
    if not parent.is_dir() or parent.is_relative_to(expected_config.resolve()):
        parser.error('artifact parent must be an existing directory outside CCS configuration')
    directory = Path(tempfile.mkdtemp(prefix='ccs-controlled-', dir=parent))
    os.chmod(directory, 0o700)
    save_json(directory / 'plan.json', plan)
    environment = {'HOME': str(real_home), 'USER': pwd.getpwuid(os.getuid()).pw_name,
                   'PATH': '/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin',
                   'LANG': 'en_US.UTF-8', 'RUST_LOG': 'info',
                   'CLAUDE_CODE_SYNC_CONFIG_DIR': str(args.config_dir)}
    print('ARTIFACT_ROOT=' + str(directory), flush=True)
    result = run_observed([str(binary), 'push', '--scheduled'], environment, real_home,
                          directory, args.timeout, args.term_grace)
    print(json.dumps({'outcome': result['outcome'], 'exit_code': result['exit_code'],
                      'residual_process_group': result['residual_process_group']}))
    return result['exit_code']


if __name__ == '__main__':
    raise SystemExit(main())
