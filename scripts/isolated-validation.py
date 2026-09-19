#!/usr/bin/env python3
"""Fail-closed macOS validation, with retained artifacts and no production writes.

Examples (run from anywhere):
  python3 scripts/isolated-validation.py --self-check
  python3 scripts/isolated-validation.py --root-parent /Volumes/Data -- cargo test --lib project_roots::tests::production_probe_accepts_private_fixture_subdirectory -- --exact --ignored
  python3 scripts/isolated-validation.py -- target/debug/ccs --help

Each invocation creates a private root, prints its location, runs safety probes,
then optionally executes ONE explicit argv (never through a shell). Every child
gets a clean environment. CCS_TEST_VOLUME_FIXTURE points at root/volume-fixture.
File writes are allowed only below that root and the repository's physical target;
/dev/null additionally permits file-write-data for child Stdio::null.
Toolchain and existing Cargo registry caches are readable, NOT writable. Network
is denied. Canonical launchctl, root copies and symlink aliases are exec-denied.
This is not hostile-code confinement: target executables and custom service IPC
remain trusted. Schedule behavior MUST use injected runners; schedule CLI is refused.
No artifacts are deleted. This is a write-safety boundary, not a secrets sandbox:
reads and system IPC remain available for diskutil/plutil and the toolchain.
Children have bounded timeouts and private process groups; cleanup sends SIGTERM
only and records residual groups. Outside-root denial probes touch only fresh
private synthetic sentinels; production paths receive read-only policy queries.
"""

import argparse
import ctypes
import datetime
import hashlib
import json
import os
from pathlib import Path
import pwd
import shutil
import signal
import subprocess
import sys
import tempfile
import time


# Executed under the SAME sandbox/environment as the requested command. Querying
# policy does not open, create, truncate, chmod, or otherwise touch protected data.
PROBE = r'''
import ctypes, errno, hashlib, json, os, pathlib, socket, subprocess, sys
root = pathlib.Path(sys.argv[1])
protected = json.loads(sys.argv[2])
fixture = json.loads(sys.argv[3])
lib = ctypes.CDLL('/usr/lib/libsandbox.dylib', use_errno=True)
check = lib.sandbox_check
check.restype = ctypes.c_int
check.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int]
def policy(operation, path):
    result = check(os.getpid(), operation.encode(), 1, ctypes.c_char_p(os.fsencode(path)))
    if result < 0:
        raise RuntimeError('sandbox policy query failed: ' + repr((operation, path, ctypes.get_errno())))
    return result
results = []
with open('/dev/null', 'wb') as null_device:
    null_device.write(b'isolated null-device canary\n')
results.append({'probe': 'null-device-write', 'path': '/dev/null', 'result': 'allowed'})
allowed = root / 'allowed-write-probe'
allowed.write_text('isolated write succeeded\n')
assert allowed.read_text() == 'isolated write succeeded\n'
results.append({'probe': 'isolated-write', 'path': str(allowed), 'result': 'allowed'})
target_probe = pathlib.Path(fixture['target_probe'])
target_probe.write_text('isolated target write succeeded\n')
results.append({'probe': 'target-write', 'path': str(target_probe), 'result': 'allowed'})
def denied_write(path):
    try:
        with open(path, 'wb') as stream:
            stream.write(b'canary must not escape')
    except OSError as error:
        assert error.errno in (errno.EPERM, errno.EACCES), repr(error)
    else:
        raise RuntimeError('outside-root write allowed: ' + str(path))
for path in fixture['denied_write_paths']:
    denied_write(path)
results.append({'probe': 'outside-root-and-symlink-write', 'result': 'denied'})
sentinel = pathlib.Path(fixture['sentinel'])
assert hashlib.sha256(sentinel.read_bytes()).hexdigest() == fixture['sentinel_sha256']
assert not pathlib.Path(fixture['outside_new']).exists()
descendant_code = """import errno, pathlib, sys
try:
    pathlib.Path(sys.argv[1]).write_bytes(b'descendant must not escape')
except OSError as error:
    assert error.errno in (errno.EPERM, errno.EACCES)
else:
    raise RuntimeError('descendant escaped sandbox')
pathlib.Path(sys.argv[2]).write_text('descendant inherited sandbox')
"""
child = subprocess.run([sys.executable, '-c', descendant_code, str(sentinel), str(root / 'descendant-proof')])
assert child.returncode == 0
results.append({'probe': 'descendant-inheritance', 'result': 'denied'})
with socket.socket() as connection:
    connection.settimeout(1)
    try:
        connection.connect(('127.0.0.1', 9))
    except OSError as error:
        assert error.errno in (errno.EPERM, errno.EACCES), repr(error)
    else:
        raise RuntimeError('network unexpectedly allowed')
results.append({'probe': 'network-outbound', 'result': 'denied'})
print(json.dumps({'completed_private_canaries': results}), flush=True)
for path in protected:
    for operation in ('file-write-data', 'file-write-create', 'file-write-unlink'):
        assert policy(operation, path) != 0, (operation, path, 'unexpectedly allowed')
    results.append({'probe': 'production-write-policy', 'path': path, 'result': 'denied'})
print(json.dumps({'completed_policy_queries': results}), flush=True)
for executable in ['/bin/launchctl', fixture['launchctl_alias'], fixture['launchctl_copy']]:
    # sandbox_check(process-exec, FILTER_PATH) returns EINVAL on this macOS.
    # A harmless help invocation provides the actual exec-denial proof instead.
    try:
        subprocess.run([executable, 'help'], check=False, capture_output=True)
    except OSError as error:
        assert error.errno in (errno.EPERM, errno.EACCES), repr(error)
        results.append({'probe': 'launchctl-exec', 'path': executable, 'result': 'denied', 'errno': error.errno})
    else:
        raise RuntimeError('launchctl exec was not blocked')
print(json.dumps(results, indent=2))
'''


def save_json(path, value):
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + '\n')


def run_child(root, label, argv, environment, profile, cwd, timeout):
    directory = root / label
    directory.mkdir()
    invocation = ['/usr/bin/sandbox-exec', '-f', str(profile), *argv]
    metadata = {'argv': argv, 'sandbox_argv': invocation, 'cwd': str(cwd),
                'environment': environment, 'timeout_seconds': timeout,
                'started_at': datetime.datetime.now(datetime.timezone.utc).isoformat()}
    save_json(directory / 'metadata.json', metadata)
    with (directory / 'stdout.log').open('wb') as stdout, (directory / 'stderr.log').open('wb') as stderr:
        try:
            process = subprocess.Popen(invocation, cwd=cwd, env=environment,
                                       start_new_session=True, stdin=subprocess.DEVNULL,
                                       stdout=stdout, stderr=stderr)
            metadata['private_pgid'] = process.pid
            try:
                code = process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                metadata['timed_out'] = True
                code = 124
            # Even a successful leader may leave children behind. Terminate only
            # this invocation's private group; never escalate to SIGKILL.
            try:
                os.killpg(process.pid, 0)
            except ProcessLookupError:
                metadata['residual_process_group'] = False
            else:
                os.killpg(process.pid, signal.SIGTERM)
                metadata['term_sent'] = True
                deadline = time.monotonic() + 3
                while time.monotonic() < deadline:
                    process.poll()
                    try:
                        os.killpg(process.pid, 0)
                    except ProcessLookupError:
                        break
                    time.sleep(0.05)
                try:
                    os.killpg(process.pid, 0)
                except ProcessLookupError:
                    metadata['residual_process_group'] = False
                else:
                    metadata['residual_process_group'] = True
                code = code or 125
            metadata['leader_returncode'] = process.poll()
        except OSError as error:
            stderr.write(str(error).encode())
            metadata['execution_error'] = str(error)
            code = 125
    metadata['exit_code'] = code
    metadata['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    save_json(directory / 'metadata.json', metadata)
    (directory / 'exit-code.txt').write_text(str(code) + '\n')
    print(f'{label}: exit={code}; logs={directory}', flush=True)
    return code


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--self-check', action='store_true', help='only run isolation probes')
    parser.add_argument('--timeout', type=int, default=120, help='child timeout in seconds (1..1800); TERM only')
    parser.add_argument('--root-parent', type=Path, default=Path('/private/tmp'),
                        help='existing parent for a new unique private root; use /Volumes/Data for APFS fixture')
    parser.add_argument('command', nargs=argparse.REMAINDER, help='explicit argv after --')
    args = parser.parse_args()
    if not 1 <= args.timeout <= 1800:
        parser.error('--timeout must be between 1 and 1800 seconds')
    command = args.command
    if command[:1] == ['--']:
        command = command[1:]
    if args.self_check and command:
        parser.error('--self-check cannot be combined with a command')
    if not args.self_check and not command:
        parser.error('provide --self-check or an explicit command after --')
    if sys.platform != 'darwin' or not Path('/usr/bin/sandbox-exec').is_file():
        parser.error('macOS sandbox-exec is required; refusing unprotected execution')
    # sys.executable may be Apple's launcher. dyld identifies the exact Mach-O
    # already running, without spawning xcrun or broadening executable access.
    dyld = ctypes.CDLL(None)
    executable_path = dyld._NSGetExecutablePath
    executable_path.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint32)]
    executable_path.restype = ctypes.c_int
    size = ctypes.c_uint32(0)
    executable_path(None, ctypes.byref(size))
    buffer = ctypes.create_string_buffer(size.value)
    if executable_path(buffer, ctypes.byref(size)) != 0:
        parser.error('cannot identify running Python executable; refusing execution')
    python_executable = str(Path(os.fsdecode(buffer.value)).resolve(strict=True))
    if command and any(word in {'schedule', 'enable', 'disable', 'bootstrap', 'bootout', '--enable', '--disable'} for word in command):
        parser.error('service-changing CLI commands are forbidden; use injected-runner unit tests')
    repo = Path(__file__).resolve().parent.parent
    target = repo / 'target'
    if target.is_symlink():
        parser.error('repository target must not be a symlink')
    parent = args.root_parent.resolve(strict=True)
    if parent not in (Path('/private/tmp'), Path('/Volumes/Data')):
        parser.error('--root-parent must resolve exactly to /private/tmp or /Volumes/Data')
    root = Path(tempfile.mkdtemp(prefix='ccs-isolated-', dir=parent)).resolve()
    os.chmod(root, 0o700)
    print(f'ISOLATION_ROOT={root}', flush=True)
    # All denial writes target NEW synthetic data, never production paths.
    outside = Path(tempfile.mkdtemp(prefix='ccs-canary-protected-', dir='/private/tmp')).resolve()
    os.chmod(outside, 0o700)
    sentinel = outside / 'sentinel'
    sentinel.write_bytes(b'private canary original\n')
    outside_new = outside / 'must-not-exist'
    target.mkdir(exist_ok=True)
    target_canary = Path(tempfile.mkdtemp(prefix='isolation-canary-', dir=target))
    for link in (root / 'escape', target_canary / 'escape'):
        link.symlink_to(outside, target_is_directory=True)
    launchctl_alias = root / 'launchctl-alias'
    launchctl_alias.symlink_to('/bin/launchctl')
    launchctl_copy = root / 'launchctl-copy'
    shutil.copyfile('/bin/launchctl', launchctl_copy)
    launchctl_copy.chmod(0o700)
    fixture = {'sentinel': str(sentinel), 'sentinel_sha256': hashlib.sha256(sentinel.read_bytes()).hexdigest(),
               'outside_new': str(outside_new), 'target_probe': str(target_canary / 'allowed-write'),
               'launchctl_alias': str(launchctl_alias), 'launchctl_copy': str(launchctl_copy),
               'denied_write_paths': [str(sentinel), str(outside_new),
                   str(root / 'escape/sentinel'), str(target_canary / 'escape/sentinel')]}
    save_json(root / 'canary-fixtures.json', fixture)
    real_home = Path(pwd.getpwuid(os.getuid()).pw_dir).resolve()
    source_cargo = Path(os.environ.get('CARGO_HOME', str(real_home / '.cargo'))).resolve()
    source_rustup = Path(os.environ.get('RUSTUP_HOME', str(real_home / '.rustup'))).resolve()
    paths = {name: root / name for name in ('home', 'config', 'tmp', 'cache', 'cargo', 'data', 'state', 'volume-fixture')}
    for directory in paths.values():
        directory.mkdir()
    # Reuse only registry payloads. Cargo's mutable locks/database live in the
    # isolated CARGO_HOME, and registry writes still fail closed if attempted.
    registry = paths['cargo'] / 'registry'
    registry.mkdir()
    for name in ('src', 'cache', 'index'):
        source = source_cargo / 'registry' / name
        if source.is_dir():
            (registry / name).symlink_to(source, target_is_directory=True)
    cargo_bin = source_cargo / 'bin'
    environment = {
        'PATH': os.pathsep.join([str(cargo_bin), '/opt/homebrew/bin', '/usr/bin', '/bin', '/usr/sbin', '/sbin']),
        'HOME': str(paths['home']), 'USERPROFILE': str(paths['home']),
        'CLAUDE_CODE_SYNC_CONFIG_DIR': str(paths['config']),
        'CLAUDE_CONFIG_DIR': str(paths['home'] / '.claude'),
        'CODEX_HOME': str(paths['home'] / '.codex'),
        'TMPDIR': str(paths['tmp']) + '/', 'TMP': str(paths['tmp']), 'TEMP': str(paths['tmp']),
        'XDG_CONFIG_HOME': str(paths['config']), 'XDG_CACHE_HOME': str(paths['cache']),
        'XDG_DATA_HOME': str(paths['data']), 'XDG_STATE_HOME': str(paths['state']),
        'APPDATA': str(paths['config']), 'LOCALAPPDATA': str(paths['data']),
        'CARGO_HOME': str(paths['cargo']), 'RUSTUP_HOME': str(source_rustup),
        'CARGO_TARGET_DIR': str(target), 'CARGO_NET_OFFLINE': 'true',
        'CARGO_INCREMENTAL': '0', 'RUST_TEST_THREADS': '1',
        'CCS_TEST_VOLUME_FIXTURE': str(paths['volume-fixture']),
        'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': str(root / 'gitconfig'),
        'GIT_TERMINAL_PROMPT': '0', 'GIT_AUTHOR_NAME': 'Isolated validation',
        'GIT_AUTHOR_EMAIL': 'validation@example.invalid',
        'GIT_COMMITTER_NAME': 'Isolated validation', 'GIT_COMMITTER_EMAIL': 'validation@example.invalid',
        'PYTHONDONTWRITEBYTECODE': '1', 'CLANG_MODULE_CACHE_PATH': str(paths['cache'] / 'clang'),
        'LANG': 'en_US.UTF-8', 'LC_ALL': 'en_US.UTF-8',
    }
    # Literal paths are quoted, never interpolated as sandbox syntax.
    quote = json.dumps
    system_tools = ['/usr/bin/python3', '/bin/sh', '/bin/bash', '/usr/bin/env',
                    '/usr/bin/git', '/usr/bin/xcrun', '/usr/bin/clang', '/usr/bin/cc',
                    '/usr/bin/ar', '/usr/bin/ld', '/usr/bin/ranlib', '/usr/bin/diskutil',
                    '/usr/sbin/diskutil', '/usr/bin/plutil', '/usr/bin/sw_vers', '/usr/bin/uname']
    executables = sorted({str(Path(path).resolve()) for path in system_tools if Path(path).exists()}
                         | {str(Path(sys.executable).resolve()), python_executable,
                            str((cargo_bin / 'rustup').resolve())})
    exec_rules = ' '.join(f'(literal {quote(path)})' for path in executables)
    exec_rules += f' (subpath {quote(str(target))}) (subpath {quote(str(source_rustup / "toolchains"))})'
    exec_rules += ' (subpath "/Library/Developer/CommandLineTools/usr/bin")'
    profile = root / 'sandbox.sb'
    profile.write_text('\n'.join([
        '(version 1)', '(allow default)',
        '(deny file-write*)',
        f'(allow file-write* (subpath {quote(str(root))}) (subpath {quote(str(target))}))',
        '(allow file-write-data (literal "/dev/null"))',
        '(deny process-exec)',
        f'(allow process-exec {exec_rules})',
        '(deny process-exec (literal "/bin/launchctl") (literal "/usr/bin/launchctl"))',
        '(deny network*)', '',
    ]))
    protected = [str(real_home), str(real_home / '.claude'), str(real_home / '.codex'),
                 str(real_home / 'Library/Application Support/claude-code-sync'),
                 str(real_home / 'Library/Application Support/claude-code-sync/state.json'),
                 str(real_home / 'Library/Application Support/claude-code-sync/config.toml'),
                 str(real_home / 'Library/Application Support/claude-code-sync/snapshots'),
                 str(real_home / 'Library/Application Support/claude-code-sync/operation-history.json'),
                 str(repo / 'Cargo.toml'), str(parent / 'ccs-protected-nonexistent-probe')]
    inherited_config = os.environ.get('CLAUDE_CODE_SYNC_CONFIG_DIR')
    if inherited_config:
        protected.append(str(Path(inherited_config).resolve() / 'operation-history.json'))
    save_json(root / 'contract.json', {
        'root': str(root), 'writable_subtrees': [str(root), str(target)],
        'device_write_exception': {'literal': '/dev/null', 'operation': 'file-write-data'},
        'cache_reuse': 'read-only registry symlinks; isolated mutable Cargo metadata',
        'production_paths_policy_probed': protected, 'network': 'denied',
        'launchctl': 'canonical path, private-root copies and symlink aliases denied',
        'execution_boundary': 'explicit system executables plus target/toolchains; not hostile-code confinement: copied executables in target and custom service IPC are outside the guarantee; schedule only via injected runners',
        'environment_inheritance': 'none', 'timeout_seconds': args.timeout,
        'termination': 'private process group SIGTERM only; residual group blocks subsequent command',
        'read_and_system_ipc': 'allowed', 'artifacts': 'retained',
    })
    code = run_child(root, 'self-check', [python_executable, '-c', PROBE, str(root), json.dumps(protected), json.dumps(fixture)],
                     environment, profile, repo, min(args.timeout, 30))
    after_hash = hashlib.sha256(sentinel.read_bytes()).hexdigest()
    save_json(root / 'canary-parent-verification.json', {
        'before_sha256': fixture['sentinel_sha256'], 'after_sha256': after_hash,
        'outside_new_exists': outside_new.exists(), 'protected_fixture': str(outside),
    })
    if after_hash != fixture['sentinel_sha256'] or outside_new.exists():
        code = 125
    if code:
        print('Isolation self-check failed; command NOT executed.', file=sys.stderr)
        return 125
    if args.self_check:
        return 0
    executable = shutil.which(command[0], path=environment['PATH']) if '/' not in command[0] else str((repo / command[0]).resolve())
    if not executable:
        save_json(root / 'command-rejected.json', {'argv': command, 'error': 'executable not found'})
        print('Command not found in isolated PATH.', file=sys.stderr)
        return 125
    resolved = Path(executable).resolve()
    permitted = {Path(sys.executable).resolve(), Path(python_executable), (cargo_bin / 'cargo').resolve(),
                 (target / 'debug/ccs').resolve(), (target / 'release/ccs').resolve()}
    if resolved not in permitted:
        save_json(root / 'command-rejected.json', {'argv': command, 'error': 'only cargo, built ccs or this Python interpreter allowed'})
        return 125
    return run_child(root, 'command', [executable, *command[1:]], environment, profile, repo, args.timeout)


if __name__ == '__main__':
    sys.exit(main())
