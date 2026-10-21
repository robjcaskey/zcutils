#!/usr/bin/env python3
"""Check raw and ext4 synchronous writes against an owned RAM TCP fixture.

Run as root ONLY in an isolated Linux VM, with e2fsprogs installed. Refuses an
existing client device/module. Formats only the newly created 64 MiB RAM target.
Checks wire ordering and acknowledgement timing, not physical durability.
"""
import argparse
import errno
import json
import mmap
import os
import pathlib
import socket
import struct
import subprocess
import threading
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--module', type=pathlib.Path, required=True)
parser.add_argument('--out', type=pathlib.Path, required=True)
parser.add_argument('--early-ack-multiple', action='store_true',
                    help='require multi-connection early-ACK flush to fail closed')
args = parser.parse_args()
if os.geteuid() != 0 or os.uname().sysname != 'Linux':
    parser.error('requires root inside an isolated Linux VM')
if pathlib.Path('/dev/zcnblk0').exists() or pathlib.Path('/sys/module/zcnblk_client_mod').exists():
    parser.error('existing client device/module; refusing to disturb it')
args.module = args.module.resolve(strict=True)
args.out = args.out.resolve()
args.out.mkdir(parents=True, exist_ok=False)
operations, errors, streams, threads = [], [], [], []
lock = threading.Lock()
allow_sync, sync_seen, stop = threading.Event(), threading.Event(), threading.Event()
allow_sync.set()
hold_write, write_arrived = threading.Event(), threading.Event()
memory = bytearray(64 * 1024 * 1024)
listener = socket.socket()
listener.bind(('127.0.0.1', 0))
listener.listen()
listener.settimeout(.2)
port = listener.getsockname()[1]


def exact(stream, count):
    data = bytearray()
    while len(data) < count:
        part = stream.recv(count - len(data))
        if not part:
            raise EOFError()
        data.extend(part)
    return data


def serve(stream):
    try:
        stream.settimeout(10)
        stream.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        while not stop.is_set():
            header = exact(stream, 64)
            op = struct.unpack_from('<H', header, 12)[0]
            length = struct.unpack_from('<I', header, 20)[0]
            offset = struct.unpack_from('<Q', header, 24)[0]
            if header[:8] != b'ZCNBLK01' or offset + length > len(memory):
                raise ValueError('invalid frame')
            payload = exact(stream, length) if op == 1 else b''
            with lock:
                operations.append({'op': op, 'offset': offset, 'len': length})
            if op == 1:
                if args.early_ack_multiple:
                    write_arrived.set()
                    if not hold_write.wait(5):
                        raise TimeoutError('held write was not released')
                memory[offset:offset + length] = payload
                if args.early_ack_multiple:
                    continue
                response, data = 4, b''
            elif op == 2:
                response, data = 3, memory[offset:offset + length]
            elif op == 7:
                sync_seen.set()
                if not allow_sync.wait(5):
                    raise TimeoutError('SYNC ACK withheld too long')
                response, data = 8, b''
            else:
                raise ValueError('unexpected op ' + str(op))
            struct.pack_into('<H', header, 12, response)
            stream.sendall(header + data)
    except (EOFError, ConnectionError):
        pass
    except Exception as error:
        if not stop.is_set():
            errors.append(repr(error))
    finally:
        stream.close()


def accept():
    while not stop.is_set():
        try:
            stream, _ = listener.accept()
        except socket.timeout:
            continue
        except OSError:
            return
        streams.append(stream)
        thread = threading.Thread(target=serve, args=(stream,), daemon=True)
        threads.append(thread)
        thread.start()


acceptor = threading.Thread(target=accept, daemon=True)
acceptor.start()
loaded = mounted = False
fd = None
report = {'physical_durability_claim': False, 'failure': None}
log = (args.out / 'module.log').open('w')


def command(*argv, timeout=30):
    subprocess.run(argv, stdout=log, stderr=log, check=True, timeout=timeout)


try:
    command('insmod', str(args.module), 'transport=tcp', 'remote_ip=127.0.0.1',
            'remote_port_base=' + str(port), 'lanes=1', 'connections_per_lane=' + ('2' if args.early_ack_multiple else '1'),
            'queues=' + ('2' if args.early_ack_multiple else '1'), 'hctx_affinity=0', 'shard_count=1', 'size_mib=64', 'logical_block_size=4096',
            'max_frame_bytes=4096', 'queue_depth=32', 'pipeline_depth=16',
            'batch_depth=1', 'write_acks=' + ('0' if args.early_ack_multiple else '1'), 'shm_ordering_epochs=1')
    loaded = True
    for _ in range(100):
        if pathlib.Path('/dev/zcnblk0').exists():
            break
        time.sleep(.02)
    fd = os.open('/dev/zcnblk0', os.O_RDWR | os.O_DIRECT)
    if args.early_ack_multiple:
        with mmap.mmap(-1, 4096) as buffer:
            buffer[:] = b'\xa5' * 4096
            assert os.pwritev(fd, [buffer], 0) == 4096
            assert write_arrived.wait(2), 'fixture did not receive the write'
            # Round-robin queues send this flush on the other connection.
            # The first connection has received but not applied the write.
            try:
                os.fsync(fd)
                flush_errno = 0
            except OSError as error:
                flush_errno = error.errno
            report['early_ack_flush_errno'] = flush_errno
            hold_write.set()
            assert flush_errno == errno.EIO, (
                'multi-connection early-ACK flush must fail closed')
            print('PASS: multi-connection early-ACK flush fails closed', flush=True)
    else:
        with mmap.mmap(-1, 4096) as buffer:
            buffer[:] = b'\xa5' * 4096
            assert os.pwritev(fd, [buffer], 0) == 4096
            os.fsync(fd)
            assert sync_seen.is_set(), 'fsync did not reach the SYNC fixture'
            sync_seen.clear()
            allow_sync.clear()
            done, result = threading.Event(), {}

            def write_sync():
                try:
                    result['bytes'] = os.pwritev(fd, [buffer], 4096, os.RWF_DSYNC)
                except OSError as error:
                    result['errno'] = error.errno
                finally:
                    done.set()

            writer = threading.Thread(target=write_sync, daemon=True)
            writer.start()
            try:
                observed = sync_seen.wait(2)
                time.sleep(.1)
                early = done.is_set()
            finally:
                allow_sync.set()
                writer.join(timeout=5)
            report['raw_dsync'] = dict(sync_observed=observed,
                                      completed_before_sync_ack=early, result=result)
            assert not writer.is_alive(), 'raw synchronous writer did not finish'
            assert observed and not early, 'RWF_DSYNC did not wait for SYNC ACK'
            assert result.get('bytes') == 4096, result
        os.close(fd)
        fd = None
        command('mkfs.ext4', '-q', '-F', '-b', '4096', '-m', '0', '-E',
                'nodiscard,lazy_itable_init=0,lazy_journal_init=0', '/dev/zcnblk0')
        mountpoint = args.out / 'mnt'
        mountpoint.mkdir()
        command('mount', '-o', 'nodelalloc', '/dev/zcnblk0', str(mountpoint))
        mounted = True
        start = len(operations)
        fd = os.open(mountpoint / 'sync-probe.bin', os.O_WRONLY | os.O_CREAT | os.O_DSYNC, 0o600)
        assert os.write(fd, b'\x6b' * 4096) == 4096
        # Snapshot before close/unmount can add their own unrelated flushes.
        wire = list(operations[start:])
        report['filesystem_wire_ops_at_completion'] = wire
        writes = [i for i, op in enumerate(wire) if op['op'] == 1]
        syncs = [i for i, op in enumerate(wire) if op['op'] == 7]
        assert writes and syncs and max(syncs) > max(writes), (
            'ext4 O_DSYNC journal commit completed without a post-write SYNC')
        assert not errors, errors
        os.close(fd)
        fd = None
        assert (mountpoint / 'sync-probe.bin').read_bytes() == b'\x6b' * 4096
        command('umount', str(mountpoint))
        mounted = False
        command('mount', '/dev/zcnblk0', str(mountpoint))
        mounted = True
        assert (mountpoint / 'sync-probe.bin').read_bytes() == b'\x6b' * 4096
        report['filesystem_remount_readback'] = True
        print('PASS: raw SYNC ACK timing and ext4 journal postflush/remount', flush=True)
except BaseException as error:
    report['failure'] = repr(error)
    raise
finally:
    allow_sync.set()
    hold_write.set()
    try:
        if fd is not None:
            os.close(fd)
        if mounted:
            command('umount', str(args.out / 'mnt'))
        if loaded:
            # udev may briefly hold the newly announced disk open.
            for attempt in range(50):
                result = subprocess.run(['rmmod', 'zcnblk_client_mod'],
                                        stdout=log, stderr=log, timeout=15)
                if result.returncode == 0:
                    break
                time.sleep(.1)
            else:
                result.check_returncode()
    finally:
        stop.set()
        listener.close()
        for stream in streams:
            try:
                stream.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        acceptor.join(timeout=1)
        for thread in threads:
            thread.join(timeout=1)
        report['server_errors'] = errors
        (args.out / 'observation.json').write_text(json.dumps(report, indent=2) + '\n')
        log.close()
