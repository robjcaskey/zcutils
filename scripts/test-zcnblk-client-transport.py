#!/usr/bin/env python3
"""Isolated Linux VM integration test: TCP admission and fail-closed SHM.

Requires root, fio, a module built for the running kernel, and a portable target.
Refuses an existing zcnblk device/module. All successful writes go to an owned
userspace RAM leaf. This is correctness coverage, not a performance benchmark.
"""
import argparse,errno,json,os,pathlib,socket,subprocess,time
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--module',type=pathlib.Path,required=True);p.add_argument('--target',type=pathlib.Path,required=True);p.add_argument('--out',type=pathlib.Path,required=True)
a=p.parse_args()
if os.geteuid()!=0 or os.uname().sysname!='Linux':p.error('requires root inside an isolated Linux VM')
if pathlib.Path('/dev/zcnblk0').exists() or pathlib.Path('/sys/module/zcnblk_client_mod').exists():p.error('existing client device/module; refusing to disturb it')
for path in [a.module,a.target]:
 if not path.is_file():p.error('missing artifact: '+str(path))
a.module=a.module.resolve();a.target=a.target.resolve();a.out.mkdir(parents=True,exist_ok=False)
loaded=False;server=None;log=(a.out/'commands.log').open('w')
def absent():
 assert not pathlib.Path('/dev/zcnblk0').exists() and not pathlib.Path('/sys/module/zcnblk_client_mod').exists()
def unload():
 global loaded
 subprocess.run(['rmmod','zcnblk_client_mod'],stdout=log,stderr=log,check=True,timeout=15);loaded=False;absent()
def load(transport,*extra,expected=0):
 global loaded
 absent();cmd=['insmod',str(a.module),'transport='+transport,'lanes=1','connections_per_lane=1','shard_count=1','size_mib=64','max_frame_bytes=4096','queues=1','queue_depth=32','pipeline_depth=16','batch_depth=1',*extra]
 result=subprocess.run(cmd,stdout=log,stderr=log,timeout=15)
 if expected:
  assert result.returncode!=0,'invalid SHM configuration was accepted';absent()
 else:
  result.check_returncode();loaded=True
  assert pathlib.Path('/sys/module/zcnblk_client_mod').exists()
  for _ in range(100):
   if pathlib.Path('/dev/zcnblk0').exists():break
   time.sleep(.02)
  else:raise RuntimeError('client device did not appear')
def fio(name,mode,expected_error=0,verify=False):
 cmd=['timeout','--kill-after=3','20','fio','--name='+name,'--filename=/dev/zcnblk0','--rw='+mode,'--bs=4k','--size=8M' if verify else '--size=4k','--ioengine=psync','--direct=1','--iodepth=1','--numjobs=1','--output-format=json','--eta=never']
 if verify:cmd+=['--verify=crc32c','--do_verify=1','--verify_fatal=1']
 result=subprocess.run(cmd,stdout=subprocess.PIPE,stderr=log,text=True,timeout=25)
 (a.out/(name+'.stdout')).write_text(result.stdout)
 report=json.loads(result.stdout[result.stdout.index('{'):]);job=report['jobs'][0]
 (a.out/(name+'.json')).write_text(json.dumps(report,indent=2)+'\n')
 assert job['error']==expected_error,(name,job['error'],expected_error)
 if expected_error:assert result.returncode!=0
 else:
  result.check_returncode();assert job['write']['io_bytes']==8*1024*1024 and job['read']['io_bytes']==8*1024*1024
try:
 load('shm','shm_ordering_epochs=0',expected=1)
 print('PASS: SHM requires ordering epochs',flush=True)
 load('shm','shm_ordering_epochs=1','shm_ring_entries=64','shm_payload_entries=64')
 fio('shm-offline-read','read',errno.EIO);fio('shm-offline-write','write',errno.EIO);unload()
 print('PASS: offline SHM rejects reads and writes with EIO',flush=True)
 with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
 server=subprocess.Popen([str(a.target),'--listen','127.0.0.1:'+str(port),'--ram-mib','64'],stdout=log,stderr=log)
 for _ in range(100):
  if server.poll() is not None:raise RuntimeError('RAM leaf exited')
  try:
   with socket.create_connection(('127.0.0.1',port),.1):break
  except OSError:time.sleep(.02)
 else:raise RuntimeError('RAM leaf did not listen')
 load('tcp','remote_ip=127.0.0.1','remote_port_base='+str(port),'write_acks=1','shm_ordering_epochs=1')
 fio('tcp-crc-write-read','write',verify=True)
 fd=os.open('/dev/zcnblk0',os.O_RDWR)
 try:os.fsync(fd)
 finally:os.close(fd)
 unload()
 print('PASS: TCP with SHM epochs enabled completes 8MiB CRC write/read and visibility flush',flush=True)
 (a.out/'status.json').write_text(json.dumps({'returncode':0,'tcp_crc_bytes_each_direction':8*1024*1024,'shm_offline_read_write':'EIO','shm_epochs_required':True,'tcp_ram_visibility_flush':True,'representative':False})+'\n')
finally:
 if loaded:
  try:unload()
  except Exception:
   if server and server.poll() is None:server.terminate();server.wait(timeout=5)
   subprocess.run(['rmmod','zcnblk_client_mod'],stdout=log,stderr=log,timeout=15,check=True)
 if server and server.poll() is None:server.terminate();server.wait(timeout=5)
 log.close()
