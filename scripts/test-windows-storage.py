#!/usr/bin/env python3
"""Windows: real TCP WAL crash recovery and full-image backup/restore."""
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
    return subprocess.check_output([str(BIN / (name + '.exe')), *map(str, args)], text=True, env=env, timeout=45)


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
        p = subprocess.Popen([str(BIN / (name + '.exe')), *map(str, args)], stdout=log, stderr=subprocess.STDOUT, env=env)
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
        failed = subprocess.run([str(BIN / 'zcnblk-client.exe'), 'backup', source,
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

    finally:
        for process in reversed(processes):
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=15)
        for handle in handles:
            handle.close()
