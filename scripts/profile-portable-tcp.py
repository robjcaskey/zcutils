#!/usr/bin/env python3
"""Mac stack sampling for an owned target/client. Timings are not benchmarks."""
import argparse,hashlib,json,pathlib,socket,subprocess,time
p=argparse.ArgumentParser();p.add_argument('--probe',required=True);p.add_argument('--target',required=True);p.add_argument('--out',required=True,type=pathlib.Path);a=p.parse_args()
a.out.mkdir(parents=True,exist_ok=False)
meta={'profiled_not_benchmark':True,'workload':'1 MiB verified random read, one lane/worker, QD64, CPU OS-scheduled','target_sha256':hashlib.sha256(pathlib.Path(a.target).read_bytes()).hexdigest(),'probe_sha256':hashlib.sha256(pathlib.Path(a.probe).read_bytes()).hexdigest()};(a.out/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
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
  client=subprocess.Popen(base+['read-verify','200000','1','64','1048576','268435456','1','random','volatile-ram'],stdout=output,stderr=log);children.append(client)
  time.sleep(1)
  samples=[start(['sample',str(target.pid),'5','1','-file',str(a.out/'target-sample.txt')]),start(['sample',str(client.pid),'5','1','-file',str(a.out/'client-sample.txt')])]
  for sample in samples:sample.wait(timeout=45)
  client.wait(timeout=120)
  if client.returncode!=0:raise RuntimeError('profiled client failed')
 (a.out/'status.json').write_text(json.dumps({'returncode':0,'target_pid':target.pid,'client_pid':client.pid,'profiled_not_benchmark':True})+'\n')
finally:
 for child in reversed(children):stop(child)
 log.close()
