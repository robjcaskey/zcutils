#!/usr/bin/env python3
"""Repeat portable RAM-leaf TCP probes. Exploratory, shared-system measurements.

Starts only disposable RAM and echo services on loopback; owns exact child PIDs.
No block devices, cloud resources, topology mutation, or scheduled jobs.
"""
import argparse,json,os,pathlib,platform,socket,statistics,subprocess,time
p=argparse.ArgumentParser()
p.add_argument('--probe',required=True);p.add_argument('--target',required=True);p.add_argument('--out',required=True)
p.add_argument('--seconds',type=float,default=2);p.add_argument('--repeats',type=int,default=3)
p.add_argument('--quick',action='store_true');a=p.parse_args()
if a.seconds<=0 or a.repeats<3: p.error('positive duration and >=3 repeats required')
out=pathlib.Path(a.out);out.mkdir(parents=True,exist_ok=False)
meta={'platform':platform.platform(),'machine':platform.machine(),'cpus':os.cpu_count(),'representative':False,'topology':'TCP loopback; terminal RAM leaf; thread-per-connection target; one sender and receiver per client lane; CPUs OS-scheduled; no hctx, RDMA, hugepages, memlock reservation or io_uring','args':vars(a)}
(out/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
children=[]
log=(out/'services.log').open('w')
trace=(out/'probe-stderr.log').open('w')
def address():
 with socket.socket() as s:s.bind(('127.0.0.1',0));return '127.0.0.1:'+str(s.getsockname()[1])
def start(cmd,addr):
 child=subprocess.Popen(cmd,stdout=log,stderr=subprocess.STDOUT);children.append(child)
 for _ in range(100):
  if child.poll() is not None:raise RuntimeError('service exited: '+str(cmd))
  try:
   with socket.create_connection(('127.0.0.1',int(addr.split(':')[1])),.1):return
  except OSError:time.sleep(.05)
 raise RuntimeError('service did not listen')
def invoke(*args):
 return json.loads(subprocess.check_output([a.probe,*map(str,args)],stderr=trace,text=True,timeout=180))
try:
 leaf,echo=address(),address();start([a.target,'--listen',leaf,'--ram-mib','256'],leaf);start([a.probe,'echo-server',echo],echo)
 rtt=invoke('rtt',echo,1001);(out/'rtt.json').write_text(json.dumps(rtt)+'\n')
 # Deterministic byte-position patterns remain valid across request sizes and lanes.
 invoke('run',leaf,'write',4096,1,32,65536,256*1024*1024,rtt['min_us'],'sequential','volatile-ram')
 results=[]
 for size in ([4096] if a.quick else [4096,65536,1048576]):
  for workers in ([1] if a.quick else [1,4]):
   for qd in ([1,16] if a.quick else [1,2,4,8,16,64,256]):
    for mode in ['read-verify','write']:
     calops=max(128,min(8192,8*1024*1024//size*qd))
     cal=invoke('run',leaf,mode,calops,workers,qd,size,256*1024*1024,rtt['min_us'],'random','volatile-ram')
     ops=max(128,min(4_000_000,8*1024*1024*1024//size,int(calops*a.seconds/max(cal['elapsed_s'],.001))))
     rows=[]
     for repeat in range(a.repeats):
      record=invoke('run',leaf,mode,ops,workers,qd,size,256*1024*1024,rtt['min_us'],'random','volatile-ram')
      record['repeat']=repeat;rows.append(record)
      with (out/'results.jsonl').open('a') as f:f.write(json.dumps(record)+'\n')
     rates=[row['iops'] for row in rows]
     summary={'io_bytes':size,'workers':workers,'per_worker_qd':qd,'aggregate_depth':workers*qd,'operation':mode,'iops_min':min(rates),'iops_median':statistics.median(rates),'iops_max':max(rates),'raw_tcp_echo_rtt_us':rtt['min_us'],'rtt_reference_statistic':'minimum of 1001 warmed echo samples','rtt_reference_valid':all(row['actual_theoretical_efficiency']<=1 for row in rows),'rtt_iops_ceiling':rows[0]['rtt_iops_ceiling'],'median_efficiency':statistics.median(row['actual_theoretical_efficiency'] for row in rows),'representative':False}
     results.append(summary);(out/'summary.json').write_text(json.dumps(results,indent=2)+'\n')
     print(json.dumps(summary),flush=True)
finally:
 for child in reversed(children):
  if child.poll() is None:child.terminate()
  try:child.wait(timeout=10)
  except subprocess.TimeoutExpired:child.kill();child.wait(timeout=5)
 log.close();trace.close()
