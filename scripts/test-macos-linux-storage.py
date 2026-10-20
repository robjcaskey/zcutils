#!/usr/bin/env python3
"""Run real mixed Linux/macOS storage and regional tests over one SSH tunnel.

Usage: test-macos-linux-storage.py SSH_WRAPPER OUTPUT_DIRECTORY
SSH_WRAPPER must execute ssh to the Mac, forwarding command arguments unchanged.
The Mac checkout is ~/zcutils; binaries are target/debug on both hosts.
"""
import hashlib
import json
import os
import pathlib
import shlex
import subprocess
import sys
import time

ssh = str(pathlib.Path(sys.argv[1]).resolve())
root = pathlib.Path(sys.argv[2]).resolve()
root.mkdir(parents=True, exist_ok=True)
bin_dir = pathlib.Path('target/debug').resolve()
remote_root = '/Users/ec2-user/zcutils-cross-test-' + str(int(time.time()))
processes, files = [], []
remote_pids = []


def remote(command, data=None):
    return subprocess.check_output([ssh, command], input=data, timeout=60)


def mac_run(name, *args):
    return remote('cd ~/zcutils && ' + shlex.join(['target/debug/' + name, *map(str, args)])).decode()


def linux_run(name, *args, env=None):
    return subprocess.check_output([str(bin_dir / name), *map(str, args)], env=env, text=True, stderr=subprocess.PIPE, timeout=45)


def start(name, args, mac=False, env=None):
    log = (root / f'process-{len(processes)}.log').open('w')
    files.append(log)
    if mac:
        pid = remote_root + '/' + name + '-' + str(len(processes)) + '.pid'
        remote_pids.append(pid)
        prefix = shlex.join(['env', *[f'{k}={v}' for k, v in (env or {}).items()]]) + ' ' if env else ''
        command = ('echo $$ > ' + shlex.quote(pid) + '; cd ~/zcutils; exec ' + prefix
                   + shlex.join(['target/debug/' + name, *map(str, args)]))
        p = subprocess.Popen([ssh, command], stdout=log, stderr=subprocess.STDOUT)
    else:
        p = subprocess.Popen([str(bin_dir / name), *map(str, args)], env=env, stdout=log, stderr=subprocess.STDOUT)
    p.log_path = pathlib.Path(log.name)
    processes.append(p)
    return p


def wait_log_ready(process):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if 'RACING_MIRROR_READY' in process.log_path.read_text():
            return
        if process.poll() is not None:
            raise RuntimeError(process.log_path.read_text())
        time.sleep(.05)
    raise TimeoutError(process.log_path.read_text())


def wait_service(addr):
    for _ in range(80):
        try:
            linux_run('zcnblk-client', 'sync', addr)
            return
        except subprocess.CalledProcessError:
            time.sleep(.1)
    raise RuntimeError('target did not become ready: ' + addr)


try:
    remote('mkdir -p ' + shlex.quote(remote_root))
    # All data endpoints remain on loopback; the tunnel protects public transit.
    forward = [29702, 29704, 29705, 29712]
    reverse = [29701, 29703, 29706, 29711, 29713]
    tunnel_args = ['-N', '-o', 'ExitOnForwardFailure=yes']
    for port in forward:
        tunnel_args += ['-L', f'127.0.0.1:{port}:127.0.0.1:{port}']
    for port in reverse:
        tunnel_args += ['-R', f'127.0.0.1:{port}:127.0.0.1:{port}']
    tunnel_log = (root / 'tunnel.log').open('w')
    files.append(tunnel_log)
    tunnel = subprocess.Popen([ssh, *tunnel_args], stdout=tunnel_log, stderr=subprocess.STDOUT)
    processes.append(tunnel)
    time.sleep(2)
    assert tunnel.poll() is None, 'SSH tunnel exited'

    if os.environ.get('ZCUTILS_TEST_REGIONS_ONLY') != '1':
        for mac, port, base in [(True, 29705, remote_root), (False, 29706, str(root))]:
            start('zcnblk-portable-target', ['--listen', f'127.0.0.1:{port}', '--file', base + '/base.img',
                  '--size-mib', 8, '--wal-journal', base + '/journal.img', '--journal-mib', 4], mac=mac)
            wait_service(f'127.0.0.1:{port}')
        payload = bytes((i * 17 + 9) % 256 for i in range(4096 * 65))
        source = root / 'input.bin'
        source.write_bytes(payload)
        linux_run('zcnblk-client', 'verify', '127.0.0.1:29705', 8192, source, root / 'mac-readback.bin')
        linux_run('zcnblk-client', 'backup', '127.0.0.1:29705', 8 * 1024 * 1024, root / 'mac-backup.img', '--source-quiesced')
        linux_run('zcnblk-client', 'restore', '127.0.0.1:29706', root / 'mac-backup.img')
        remote('cat > ' + shlex.quote(remote_root + '/input.bin'), payload)
        mac_run('zcnblk-client', 'verify', '127.0.0.1:29706', 4096, remote_root + '/input.bin', remote_root + '/linux-readback.bin')
        mac_run('zcnblk-client', 'backup', '127.0.0.1:29706', 8 * 1024 * 1024, remote_root + '/linux-backup.img', '--source-quiesced')
        mac_run('zcnblk-client', 'restore', '127.0.0.1:29705', remote_root + '/linux-backup.img')
        linux_run('zcnblk-client', 'read', '127.0.0.1:29705', 4096, len(payload), root / 'restored-from-linux.bin')
        assert (root / 'restored-from-linux.bin').read_bytes() == payload
        print('MIXED_MAC_LINUX_WAL_BACKUP_RESTORE_PASS directions=both sha256=' + hashlib.sha256(payload).hexdigest(), flush=True)

        # Exercise each platform as the mirror first hop and the terminal leaf.
        for mac_hop, leaf_port, hop_port in [(True, 29701, 29702), (False, 29704, 29703)]:
            local_log = (remote_root if mac_hop else str(root)) + '/hop.log'
            leaf_log = (str(root) if mac_hop else remote_root) + '/leaf.log'
            leaf = start('zcracing-mirror', ['leaf', f'127.0.0.1:{leaf_port}', leaf_log], mac=not mac_hop)
            wait_log_ready(leaf)
            hop = start('zcracing-mirror', ['first-hop', f'127.0.0.1:{hop_port}', f'127.0.0.1:{leaf_port}', local_log], mac=mac_hop)
            wait_log_ready(hop)
            if mac_hop:
                linux_run('zcracing-mirror', 'client', f'127.0.0.1:{hop_port}', 17, 4096, 4)
            else:
                mac_run('zcracing-mirror', 'client', f'127.0.0.1:{hop_port}', 17, 4096, 4)
            assert hop.wait(timeout=30) == 0
            assert leaf.wait(timeout=30) == 0
            for mac, path in [(mac_hop, local_log), (not mac_hop, leaf_log)]:
                (mac_run if mac else linux_run)('zcracing-mirror', 'verify', path, 17)
            print('MIXED_MAC_LINUX_MIRROR_PASS mac_role=' + ('first-hop' if mac_hop else 'leaf') + ' durable_ack=both-legs', flush=True)

    # The same encrypted regional RPC protocol crosses the tunnel in both directions.
    credential = root / 'credentials.json'
    linux_run('zcglobal-policy-node', 'credential-init', credential, '10m')
    remote('cat > ' + shlex.quote(remote_root + '/credentials.json') + '; chmod 600 ' + shlex.quote(remote_root + '/credentials.json'), credential.read_bytes())
    addresses = [f'127.0.0.1:{port}' for port in [29711, 29712, 29713]]
    peers = ','.join(f'{name}@{addr}#leader' for name, addr in zip('abc', addresses))
    env = os.environ.copy()
    env.update(ZCGLOBAL_FEDERATION_ID='mixed-platform', ZCGLOBAL_ADMIN_TOKEN_FILE=str(credential), ZCGLOBAL_TIMING_SCALE='5')
    nodes = []
    for name, addr in zip('abc', addresses):
        mac = name == 'b'
        base = remote_root if mac else str(root)
        service_env = {'ZCGLOBAL_FEDERATION_ID': 'mixed-platform', 'ZCGLOBAL_ADMIN_TOKEN_FILE': base + '/credentials.json', 'ZCGLOBAL_TIMING_SCALE': '5'} if mac else env
        nodes.append(start('zcglobal-policy-node', ['serve', 'mixed-platform', name, 'mac-region' if mac else 'linux-region', addr, base + '/' + name + '.json', peers, base + '/credentials.json'], mac=mac, env=service_env))
    deadline = time.monotonic() + 45
    leader = None
    observations = []
    observations_path = root / "election-observations.json"
    while time.monotonic() < deadline:
        statuses = []
        for i, addr in enumerate(addresses):
            try:
                status = json.loads(linux_run('zcglobal-policy-node', 'status', addr, env=env))['status']
                statuses.append(status)
                observations.append({"node":i,"status":status})
                if status['role'] == 'leader' and status['authority_valid']:
                    leader = i
            except (subprocess.CalledProcessError, KeyError) as error:
                observations.append({"node":i,"error":str(error), "stderr":str(getattr(error,"stderr",""))})
        observations_path.write_text(json.dumps(observations, indent=2))
        if leader is not None and len(statuses) == 3:
            break
        time.sleep(.2)
    (root / "election-observations.json").write_text(json.dumps(observations, indent=2))
    assert leader is not None
    linux_run('zcglobal-policy-node', 'set-rate', addresses[leader], 1, 1000, 2000,
              'mac-region:500:1500:1,linux-region:500:1500:1', 'mac-region:1200,linux-region:800', 1, 2, env=env)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        statuses = [json.loads(linux_run('zcglobal-policy-node', 'status', a, env=env))['status'] for a in addresses]
        if all(s['policy_revision'] == 1 for s in statuses):
            break
        time.sleep(.2)
    else:
        raise AssertionError('policy did not commit to the Mac voter')
    (root / 'regional-status.json').write_text(json.dumps(statuses, indent=2))
    print('MIXED_MAC_LINUX_REGIONAL_PASS voters=3 mac_voters=1 policy_revision=1', flush=True)
finally:
    # Only kill exact PIDs created by this test, never process name patterns.
    for pid_path in remote_pids:
        try:
            remote('p=$(cat ' + shlex.quote(pid_path) + '); case "$p" in *[!0-9]*|"") exit 1;; esac; kill "$p" 2>/dev/null || true')
        except subprocess.SubprocessError:
            pass
    for p in reversed(processes):
        if p.poll() is None:
            p.terminate()
    for p in processes:
        try:
            p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            p.kill(); p.wait()
    for f in files:
        f.close()
