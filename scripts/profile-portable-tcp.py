#!/usr/bin/env python3
"""Mac stack sampling for an owned target/client. Timings are not benchmarks."""
import argparse,hashlib,json,pathlib,socket,subprocess,time
p=argparse.ArgumentParser();p.add_argument('--probe',required=True);p.add_argument('--target',required=True);p.add_argument('--out',required=True,type=pathlib.Path);p.add_argument('--io-bytes',type=int,default=1048576);p.add_argument('--qd',type=int,default=64);p.add_argument('--ops',type=int,default=200000);a=p.parse_args()
if a.io_bytes<=0 or a.io_bytes>33554432 or a.io_bytes%4096 or not 1<=a.qd<=4096 or a.ops<=0:p.error('positive 4 KiB-aligned I/O up to 32 MiB, QD1..4096, and positive ops required')
a.out.mkdir(parents=True,exist_ok=False)
meta={'profiled_not_benchmark':True,'workload':'verified random read; CPU OS-scheduled','io_bytes':a.io_bytes,'workers':1,'lanes':1,'per_worker_qd':a.qd,'aggregate_depth':a.qd,'operations':a.ops,'target_sha256':hashlib.sha256(pathlib.Path(a.target).read_bytes()).hexdigest(),'probe_sha256':hashlib.sha256(pathlib.Path(a.probe).read_bytes()).hexdigest()};(a.out/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
children=[];log=(a.out/'services.log').open('w')
def addr():
 with socket.socket() as s:s.bind(('127.0.0.1',0));return '127.0.0.1:'+str(s.getsockname()[1])
def start(cmd):
 child=subprocess.Popen(cmd,stdout=log,stderr=subprocess.STDOUT);children.append(child);return child
def stop(child):
 if child.poll() is None:child.terminate()
 try:child.wait(timeout=10)
 except subprocess.TimeoutExpired:child.kill();child.wait(timeout=5)
try:
 leaf=addr();target=start([a.target,'--listen',leaf,'--ram-mib','256'])
 for _ in range(100):
  if target.poll() is not None:raise RuntimeError('target exited')
  try:
   with socket.create_connection(('127.0.0.1',int(leaf.split(':')[1])),.1):break
  except OSError:time.sleep(.05)
 else:raise RuntimeError('target did not listen')
 base=[a.probe,'run',leaf]
 subprocess.run(base+['write','4096','1','32','65536','268435456','1','sequential','volatile-ram'],stdout=subprocess.DEVNULL,stderr=log,check=True,timeout=60)
 # The probe's RTT parameter affects only reporting, which is suppressed.
 # Stack-sampled runs are not performance measurements.
 with open('/dev/null','w') as output:
  client=subprocess.Popen(base+['read-verify',str(a.ops),'1',str(a.qd),str(a.io_bytes),'268435456','1','random','volatile-ram'],stdout=output,stderr=log);children.append(client)
  time.sleep(1)
  if client.poll() is not None:raise RuntimeError('profile workload finished before sampling; increase --ops')
  samples=[start(['sample',str(target.pid),'5','1','-file',str(a.out/'target-sample.txt')]),start(['sample',str(client.pid),'5','1','-file',str(a.out/'client-sample.txt')])]
  for sample in samples:
   sample.wait(timeout=45)
   if sample.returncode!=0:raise RuntimeError('stack sampler failed')
  for name,pid in [('target',target.pid),('client',client.pid)]:
   report=(a.out/(name+'-sample.txt')).read_text()
   if 'Call graph:' not in report or ('(pid '+str(pid)+')') not in report:raise RuntimeError('missing or mismatched stack sample: '+name)
  (a.out/'process-stats.txt').write_text(subprocess.check_output(['ps','-p',str(target.pid)+','+str(client.pid),'-o','pid=,etime=,%cpu=,time=,rss=,state=,comm='],text=True))
  client.wait(timeout=120)
  if client.returncode!=0:raise RuntimeError('profiled client failed')
 (a.out/'status.json').write_text(json.dumps({'returncode':0,'target_pid':target.pid,'client_pid':client.pid,'profiled_not_benchmark':True})+'\n')
finally:
 for child in reversed(children):stop(child)
 log.close()
