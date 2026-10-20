#!/usr/bin/env python3
"""Exercise real TCP, durable restart, full-image backup/restore, and mirror recovery."""
import hashlib
import json
import os
import pathlib
import socket
import subprocess
import tempfile
import time

BIN = pathlib.Path(os.environ.get('ZCUTILS_BIN_DIR', 'target/debug')).resolve()


def address():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return '127.0.0.1:' + str(sock.getsockname()[1])


def run(name, *args, env=None):
    return subprocess.check_output([str(BIN / name), *map(str, args)], text=True, env=env, timeout=45)


def wait_ready(process, addr):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f'service exited with {process.returncode}')
        try:
            with socket.create_connection((addr.split(':')[0], int(addr.split(':')[1])), .2):
                return
        except OSError:
            time.sleep(.05)
    raise TimeoutError(addr)


# A persistent leaf must not silently use Linux /tmp when it is tmpfs.
with tempfile.TemporaryDirectory(prefix='zc-portable-', dir=os.environ.get('ZCUTILS_TEST_TMPDIR', str(BIN.parent))) as directory:
    root = pathlib.Path(directory)
    processes = []
    handles = []

    def start(name, *args, env=None):
        log = (root / f'service-{len(processes)}.log').open('w')
        handles.append(log)
        p = subprocess.Popen([str(BIN / name), *map(str, args)], stdout=log, stderr=subprocess.STDOUT, env=env)
        processes.append(p)
        return p

    try:
        source = address()
        target_args = ['--listen', source, '--file', root / 'base.img', '--size-mib', '8',
                       '--wal-journal', root / 'journal.img', '--journal-mib', '4']
        p = start('zcnblk-portable-target', *target_args)
        wait_ready(p, source)
        pattern = bytes((i * 31 + 7) % 256 for i in range(4096 * 515))
        (root / 'input').write_bytes(pattern)
        run('zcnblk-client', 'verify', source, 4096, root / 'input', root / 'readback')
        # Abrupt process loss must recover the acknowledged WAL, without reducer shutdown.
        p.kill()
        p.wait(timeout=10)
        p = start('zcnblk-portable-target', *target_args)
        wait_ready(p, source)
        run('zcnblk-client', 'read', source, 4096, len(pattern), root / 'recovered')
        assert (root / 'recovered').read_bytes() == pattern
        backup = root / 'backup.img'
        run('zcnblk-client', 'backup', source, 8 * 1024 * 1024, backup, '--source-quiesced')
        expected = b'\0' * 4096 + pattern + b'\0' * (8 * 1024 * 1024 - 4096 - len(pattern))
        assert backup.read_bytes() == expected
        # Refuse to overwrite an existing backup, including a failed second invocation.
        failed = subprocess.run([str(BIN / 'zcnblk-client'), 'backup', source,
                                 str(8 * 1024 * 1024), str(backup), '--source-quiesced'],
                                capture_output=True, timeout=15)
        assert failed.returncode != 0 and backup.read_bytes() == expected
        restored = address()
        p2 = start('zcnblk-portable-target', '--listen', restored, '--ram-mib', '8')
        wait_ready(p2, restored)
        run('zcnblk-client', 'restore', restored, backup)
        run('zcnblk-client', 'read', restored, 4096, len(pattern), root / 'restored')
        assert (root / 'restored').read_bytes() == pattern
        print('PORTABLE_WAL_BACKUP_RESTORE_PASS sha256=' + hashlib.sha256(expected).hexdigest())

        # Mirror services each accept one session; do not probe them with a TCP connect.
        # Clients retry connection; first-hop retries its leaf connection.
        local_log, remote_log = root / 'local.log', root / 'remote.log'
        for count in (33, 7, 3):
            if count == 3:
                # A remote leaf recovered only its earlier committed prefix;
                # catch it up from the first hop before accepting new frames.
                with remote_log.open("r+b") as file:
                    file.truncate(33 * (64 + 4096))
            leaf, hop = address(), address()
            remote = start('zcracing-mirror', 'leaf', leaf, remote_log)
            first = start('zcracing-mirror', 'first-hop', hop, leaf, local_log)
            run('zcracing-mirror', 'client', hop, count, 4096, 8)
            assert first.wait(timeout=15) == 0
            assert remote.wait(timeout=15) == 0
        run('zcracing-mirror', 'verify', local_log, 43)
        run('zcracing-mirror', 'verify', remote_log, 43)
        assert local_log.read_bytes() == remote_log.read_bytes()
        print('PORTABLE_MIRROR_RESTART_PASS frames=43 durable_ack=both-legs remote_suffix_replay=verified')
        # Run a real authenticated three-member regional quorum, then lose its
        # leader and prove a surviving quorum retains the committed policy.
        env = os.environ.copy()
        env['ZCGLOBAL_FEDERATION_ID'] = 'portable-smoke'
        env['ZCGLOBAL_ADMIN_TOKEN_FILE'] = str(root / 'credentials.json')
        run('zcglobal-policy-node', 'credential-init', root / 'credentials.json', '10m', env=env)
        addresses = [address() for _ in range(3)]
        peers = ','.join(f'{name}@{addr}#leader' for name, addr in zip('abc', addresses))
        nodes = [start('zcglobal-policy-node', 'serve', 'portable-smoke', name,
                       'mac-region' if name == 'a' else 'linux-region', addr,
                       root / f'{name}.json', peers, root / 'credentials.json', env=env)
                 for name, addr in zip('abc', addresses)]

        def statuses():
            result = []
            for i, addr in enumerate(addresses):
                if nodes[i].poll() is None:
                    try:
                        value = json.loads(run('zcglobal-policy-node', 'status', addr, env=env))
                        result.append((i, value['status']))
                    except (subprocess.CalledProcessError, KeyError):
                        pass
            return result

        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            leaders = [(i, status) for i, status in statuses() if status['role'] == 'leader' and status['authority_valid']]
            if len(leaders) == 1:
                leader = leaders[0][0]
                break
            time.sleep(.1)
        else:
            raise AssertionError('no regional leader')
        run('zcglobal-policy-node', 'set-rate', addresses[leader], 1, 1000, 2000,
            'mac-region:500:1500:1,linux-region:500:1500:1',
            'mac-region:1200,linux-region:800', 1, 2, env=env)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            state = statuses()
            if len(state) == 3 and all(status['policy_revision'] == 1 for _, status in state):
                break
            time.sleep(.1)
        else:
            raise AssertionError('regional policy did not reach every voter')
        nodes[leader].kill()
        nodes[leader].wait(timeout=5)
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            state = statuses()
            if (len(state) == 2 and all(status['policy_revision'] == 1 for _, status in state)
                    and any(status['role'] == 'leader' and status['authority_valid'] for _, status in state)):
                break
            time.sleep(.1)
        else:
            raise AssertionError('regional quorum did not recover after leader loss')
        print('PORTABLE_REGIONAL_QUORUM_PASS voters=3 regions=2 policy_revision=1 leader_loss=recovered')
    except BaseException:
        for handle in handles:
            handle.flush()
        for path in root.glob('service-*.log'):
            print(path.name + ':\n' + path.read_text())
        raise
    finally:
        for process in processes:
            if process.poll() is None:
                process.terminate()
        for process in processes:
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        for handle in handles:
            handle.close()
