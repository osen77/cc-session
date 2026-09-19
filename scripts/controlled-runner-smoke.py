#!/usr/bin/env python3
"""Synthetic runner proof; run ONLY through scripts/isolated-validation.py.

No ccs executable is invoked. Fake children are temporary Python files under
CCS_TEST_VOLUME_FIXTURE. Even the TERM-resistant fixture exits itself; this
script never escalates to SIGKILL. Retains artifacts for review.
"""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import time


def main():
    root = Path(os.environ['CCS_TEST_VOLUME_FIXTURE']).resolve(strict=True)
    assert root.parent.name.startswith('ccs-isolated-')
    assert Path(os.environ['HOME']).resolve().is_relative_to(root.parent)
    source = Path(__file__).with_name('controlled-production-runner.py')
    spec = importlib.util.spec_from_file_location('controlled_runner', source)
    runner = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(runner)
    work = root / 'controlled-runner-smoke'
    work.mkdir(mode=0o700)
    fake = work / 'fake.py'
    fake.write_text('''import os, pathlib, signal, subprocess, sys, time
mode = sys.argv[1]
print("synthetic stdout", flush=True)
print("CCS_PUSH_PERF event=start stage=synthetic", file=sys.stderr, flush=True)
if mode == "nonzero":
    sys.exit(17)
if mode == "timeout":
    time.sleep(15)
if mode == "resistant":
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    pathlib.Path(sys.argv[2]).write_text(str(os.getpid()))
    time.sleep(2.5)
if mode == "descendant":
    child = subprocess.Popen([sys.executable, __file__, "resistant", sys.argv[2]])
    while not pathlib.Path(sys.argv[2]).exists():
        time.sleep(.01)
if mode == "noisy":
    os.write(2, b"x" * 131072)
    os.write(2, b"\\nCCS_PUSH_PERF event=end stage=synthetic elapsed_ms=1\\n")
''')
    environment = dict(os.environ)
    results = []
    # Independent sibling: the runner must never terminate unrelated processes.
    outsider = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'],
                                start_new_session=True, env=environment)
    try:
        for mode in ('normal', 'nonzero', 'timeout', 'resistant', 'descendant', 'noisy'):
            directory = work / mode
            directory.mkdir(mode=0o700)
            marker = directory / 'ready'
            result = runner.run_observed([sys.executable, str(fake), mode, str(marker)],
                                         environment, work, directory,
                                         timeout=0.5 if mode in ('timeout', 'resistant') else 5,
                                         grace=0.15, sample_interval=0.1, max_samples=2)
            results.append(result)
            assert outsider.poll() is None, 'runner signalled an unrelated sibling'
            assert result['private_pgid'] != os.getpgrp()
            assert result['sample_count'] <= 2
            events = [json.loads(line) for line in (directory / 'events.jsonl').read_text().splitlines()]
            assert events[-1]['event'] == 'finished'
            assert json.loads((directory / 'result.json').read_text()) == result
            assert b'CCS_PUSH_PERF event=start' in (directory / 'stderr.log').read_bytes()
            if mode in ('normal', 'nonzero', 'noisy'):
                assert result['outcome'] == 'exited', result
                assert result['exit_code'] == (17 if mode == 'nonzero' else 0)
                assert not result['term_sent'] and result['residual_process_group'] == 'absent'
            elif mode == 'timeout':
                assert result['outcome'] == 'unknown' and result['timed_out']
                assert result['term_sent'] and result['exit_code'] == 124
                assert result['residual_process_group'] == 'absent'
            else:
                assert result['outcome'] == 'unknown' and result['term_sent'], result
                assert result['residual_process_group'] == 'present', result
                assert any(e['event'] == 'term_sent' for e in events)
                # Self-expiry is fixture cleanup, not runner escalation/retry.
                # Poll and reap a direct child via waitpid when possible.
                time.sleep(2.6)
                try:
                    os.waitpid(result['private_pgid'], os.WNOHANG)
                except ChildProcessError:
                    pass
            if mode == 'noisy':
                assert (directory / 'stderr.log').stat().st_size > 131072
                assert b'event=end' in (directory / 'stderr.log').read_bytes()
        missing_binary = work / 'does-not-exist' / 'ccs'
        base = [sys.executable, str(source), '--ccs', str(missing_binary),
                '--binary-sha256', '0' * 64, '--home', str(work / 'nonexistent-home'),
                '--config-dir', str(work / 'nonexistent-config'), '--artifact-parent', str(work)]
        before = set(work.iterdir())
        dry = subprocess.run(base, capture_output=True, text=True, env=environment, timeout=5)
        assert dry.returncode == 0, dry.stderr
        plan = json.loads(dry.stdout)
        assert plan['mode'] == 'dry-run' and plan['argv'] == [str(missing_binary), 'push', '--scheduled']
        assert set(work.iterdir()) == before, 'dry-run wrote artifacts'
        denied = subprocess.run([*base, '--execute'], capture_output=True, text=True, env=environment, timeout=5)
        assert denied.returncode != 0 and set(work.iterdir()) == before
        arbitrary = subprocess.run([*base, '--', '/bin/echo', 'not-allowed'],
                                   capture_output=True, text=True, env=environment, timeout=5)
        assert arbitrary.returncode != 0
        save = {'cases': results, 'dry_run': plan, 'missing_authorization_refused': True,
                'arbitrary_argv_refused': True, 'unrelated_sibling_alive': outsider.poll() is None}
        runner.save_json(work / 'smoke-result.json', save)
        print(json.dumps({'passed': True, 'artifacts': str(work), 'cases': len(results)}))
    finally:
        # This known synthetic sibling is ours; never search for or kill user PIDs.
        if outsider.poll() is None:
            outsider.terminate()
        outsider.wait(timeout=10)


if __name__ == '__main__':
    main()
