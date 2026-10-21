#!/usr/bin/env python3
"""Alternate exact target binaries with fixed, adequately long RAM workloads."""
import argparse,hashlib,json,os,pathlib,platform,socket,subprocess,time,traceback
p=argparse.ArgumentParser();p.add_argument('--probe',required=True);p.add_argument('--baseline-target',required=True);p.add_argument('--candidate-target',required=True);p.add_argument('--comparison',required=True,type=pathlib.Path);p.add_argument('--out',required=True,type=pathlib.Path);p.add_argument('--seconds',type=float,default=4)
a=p.parse_args()
if a.seconds<2:p.error('confirmation runs require at least two seconds per case')
a.out.mkdir(parents=True,exist_ok=False)
comparison=json.loads(a.comparison.read_text())['configurations'];index={(r['io_bytes'],r['workers'],r['per_worker_qd'],r['operation']):r for r in comparison}
keys=[(4096,1,1,'read-verify'),(4096,1,1,'write'),(4096,1,16,'read-verify'),(4096,1,16,'write'),(65536,1,64,'read-verify'),(65536,1,64,'write'),(1048576,1,64,'read-verify'),(1048576,1,256,'read-verify'),(1048576,4,4,'write'),(65536,1,8,'write')]
cases=[]
for key in keys:
 r=index[key];rate=max(r['baseline_iops_min_median_max'][1],r['candidate_iops_min_median_max'][1]);ops=max(128,int(rate/key[1]*a.seconds));cases.append({'io_bytes':key[0],'workers':key[1],'per_worker_qd':key[2],'operation':key[3],'ops_per_worker':ops})
variants={'baseline':str(pathlib.Path(a.baseline_target).resolve()),'candidate':str(pathlib.Path(a.candidate_target).resolve())}
for name,target in variants.items():
 root=a.out/name;root.mkdir();(root/'metadata.json').write_text(json.dumps({'configuration_count':len(cases),'args':{'repeats':3,'quick':False},'cases':cases,'platform':platform.platform(),'cpu_count':os.cpu_count(),'source_binary_sha256':hashlib.sha256(pathlib.Path(target).read_bytes()).hexdigest(),'representative':False,'topology':'loopback TCP; per-lane dedicated sender/receiver, target thread per connection; OS-scheduled CPUs; no hugepages/memlock/hctx/io_uring','order':'baseline,candidate,candidate,baseline,baseline,candidate'},indent=2)+'\n')
children=[];log=(a.out/'services.log').open('w');trace=(a.out/'probe-stderr.log').open('w')
def address():
 with socket.socket() as s:s.bind(('127.0.0.1',0));return '127.0.0.1:'+str(s.getsockname()[1])
def start(cmd,addr):
 child=subprocess.Popen(cmd,stdout=log,stderr=subprocess.STDOUT);children.append(child)
 for _ in range(100):
  if child.poll() is not None:raise RuntimeError(f'service exited {child.returncode}')
  try:
   with socket.create_connection(('127.0.0.1',int(addr.split(':')[1])),.1):return child
  except OSError:time.sleep(.05)
 raise TimeoutError(addr)
def stop(child):
 if child.poll() is None:child.terminate()
 try:child.wait(timeout=10)
 except subprocess.TimeoutExpired:child.kill();child.wait(timeout=5)
def run(*args):return json.loads(subprocess.check_output([a.probe,*map(str,args)],text=True,stderr=trace,timeout=180))
try:
 echo=address();start([a.probe,'echo-server',echo],echo)
 for cycle,name in enumerate(['baseline','candidate','candidate','baseline','baseline','candidate']):
  leaf=address();child=start([variants[name],'--listen',leaf,'--ram-mib','256'],leaf)
  rtt=run('rtt',echo,1001)
  run('run',leaf,'write',4096,1,32,65536,256*1024*1024,rtt['min_us'],'sequential','volatile-ram')
  for case in cases:
   row=run('run',leaf,case['operation'],case['ops_per_worker'],case['workers'],case['per_worker_qd'],case['io_bytes'],256*1024*1024,rtt['min_us'],'random','volatile-ram')
   row.update(variant=name,cycle=cycle,fixed_ops_confirmation=True,rtt_reference_valid=row['actual_theoretical_efficiency']<=1)
   with (a.out/name/'results.jsonl').open('a') as f:f.write(json.dumps(row)+'\n')
   print(json.dumps({'variant':name,'cycle':cycle,**case,'aggregate_depth':case['workers']*case['per_worker_qd'],'iops':row['iops'],'elapsed_s':row['elapsed_s'],'raw_tcp_echo_rtt_us':row['raw_tcp_echo_rtt_us'],'rtt_iops_ceiling':row['rtt_iops_ceiling'],'efficiency':row['actual_theoretical_efficiency'],'representative':False}),flush=True)
  stop(child)
 (a.out/'status.json').write_text(json.dumps({'returncode':0,'cycles':6,'configurations_per_variant':len(cases),'repeats_per_variant':3})+'\n')
except BaseException:
 (a.out/'status.json').write_text(json.dumps({'returncode':1,'error':traceback.format_exc()})+'\n');raise
finally:
 for child in reversed(children):stop(child)
 log.close();trace.close()
