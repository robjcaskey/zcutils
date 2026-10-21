#!/usr/bin/env python3
"""Alternating persistent-leaf tests, including per-write SYNC and crash reopen.

Ordinary file/WAL files only; all services use loopback and exact owned PIDs.
Measurements are exploratory shared-system results, with explicit completion
semantics. Never substitute these for a topology-strict Linux RDMA regression.
"""
import argparse,hashlib,json,os,pathlib,platform,socket,statistics,subprocess,time
p=argparse.ArgumentParser()
p.add_argument('--probe',required=True);p.add_argument('--baseline-target',required=True);p.add_argument('--candidate-target',required=True)
p.add_argument('--out',required=True);p.add_argument('--backend',choices=['file','wal'],default='wal');p.add_argument('--seconds',type=float,default=2)
p.add_argument('--quick',action='store_true');p.add_argument('--keep-backings',action='store_true');a=p.parse_args()
if a.seconds<=0:p.error('positive duration required')
out=pathlib.Path(a.out);out.mkdir(parents=True,exist_ok=False)
sizes=[4096] if a.quick else [4096,65536];qds=[1,4] if a.quick else [1,4,16];workers=[1] if a.quick else [1,4]
configs=len(sizes)*len(qds)*len(workers)*3;capacity=64*1024*1024
variants={'baseline':str(pathlib.Path(a.baseline_target).resolve()),'candidate':str(pathlib.Path(a.candidate_target).resolve())}
for name,target in variants.items():
 d=out/name;d.mkdir()
 meta={'configuration_count':configs,'backend':a.backend,'platform':platform.platform(),'cpu_count':os.cpu_count(),'representative':False,'source_binary_sha256':hashlib.sha256(pathlib.Path(target).read_bytes()).hexdigest(),'topology':'loopback TCP, OS-scheduled client sender/receiver per lane, OS-scheduled target thread per connection; no pinning/hugepages/memlock/io_uring; target is one terminal file or WAL','args':{'repeats':3,'quick':a.quick,'target_seconds':a.seconds,'maximum_aggregate_bytes_per_trial':64*1024*1024*1024},'timing_order':'baseline,candidate,candidate,baseline,baseline,candidate'}
 (d/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
children=[];log=(out/'services.log').open('w');trace=(out/'probe-stderr.log').open('w')
def address():
 with socket.socket() as s:s.bind(('127.0.0.1',0));return '127.0.0.1:'+str(s.getsockname()[1])
def start(cmd,addr):
 child=subprocess.Popen(cmd,stdout=log,stderr=subprocess.STDOUT);children.append(child)
 for _ in range(400):
  if child.poll() is not None:raise RuntimeError(f'service exited {child.returncode}: {cmd}')
  try:
   with socket.create_connection(('127.0.0.1',int(addr.split(':')[1])),.1):return child
  except OSError:time.sleep(.05)
 raise TimeoutError(str(cmd))
def stop(child):
 if child.poll() is None:child.terminate()
 try:child.wait(timeout=10)
 except subprocess.TimeoutExpired:child.kill();child.wait(timeout=5)
def probe(*args):
 return json.loads(subprocess.check_output([a.probe,*map(str,args)],text=True,stderr=trace,timeout=180))
def call(leaf,mode,ops,w,qd,size,rtt,sync=None,pattern='random'):
 semantics={'read-verify':a.backend+'-write-ack' if a.backend=='file' else 'wal-append-ack','write':a.backend+'-write-ack' if a.backend=='file' else 'wal-append-ack','write-sync':a.backend+'-durable-sync'}[mode]
 args=['run',leaf,mode,ops,w,qd,size,capacity,rtt,pattern,semantics]
 if mode=='write-sync':args.append(sync)
 return probe(*args)
try:
 echo=address();start([a.probe,'echo-server',echo],echo)
 for cycle,name in enumerate(['baseline','candidate','candidate','baseline','baseline','candidate']):
  directory=out/name/f'cycle-{cycle}';directory.mkdir()
  leaf=address();target=variants[name]
  args=[target,'--listen',leaf,'--file',str(directory/'base.img'),'--size-mib','64']
  if a.backend=='wal':args+=['--wal-journal',str(directory/'journal.img'),'--journal-mib','64']
  else:args+=['--create']
  child=start(args,leaf)
  rtt=probe('rtt',echo,501);(directory/'raw-rtt.json').write_text(json.dumps(rtt)+'\n')
  call(leaf,'write',capacity//65536,1,16,65536,rtt['min_us'],pattern='sequential')
  sync=probe('sync-rtt',leaf,31);(directory/'sync-reference.json').write_text(json.dumps(sync)+'\n')
  for size in sizes:
   for w in workers:
    for qd in qds:
     for mode in ['read-verify','write','write-sync']:
      calops=max(4,min(128,int(.15*1e6/max(sync['median_us'],1)/w))) if mode=='write-sync' else 128
      cal=call(leaf,mode,calops,w,qd,size,rtt['min_us'],sync['min_us'])
      ops=max(4,min(500_000,64*1024*1024*1024//size//w,int(calops*a.seconds/max(cal['elapsed_s'],.001))))
      row=call(leaf,mode,ops,w,qd,size,rtt['min_us'],sync['min_us'])
      row.update(variant=name,cycle=cycle,backend=a.backend,rtt_reference_valid=(row['actual_theoretical_efficiency']<=1 if row['actual_theoretical_efficiency'] is not None else None))
      with (out/name/'results.jsonl').open('a') as f:f.write(json.dumps(row)+'\n')
      print(json.dumps({'variant':name,'cycle':cycle,'backend':a.backend,'io_bytes':size,'workers':w,'per_worker_qd':qd,'aggregate_depth':w*qd,'operation':mode,'iops':row['iops'],'mean_completion_us':row['mean_completion_us'],'completion_semantics':row['completion_semantics'],'raw_tcp_echo_rtt_us':rtt['min_us'],'sync_reference_us':sync['min_us'],'ceiling_model':row['ceiling_model'],'rtt_iops_ceiling':row['rtt_iops_ceiling'],'efficiency':row['actual_theoretical_efficiency'],'representative':False}),flush=True)
  # Every invocation ends with validated SYNC. Terminate without a graceful WAL
  # shutdown, reopen the same files, and verify the entire image by byte position.
  stop(child)
  reopen=[x for x in args if x!='--create'];child=start(reopen,leaf)
  recovery=call(leaf,'read-verify',capacity//65536,1,16,65536,rtt['min_us'],pattern='sequential')
  (directory/'crash-reopen-readback.json').write_text(json.dumps(recovery)+'\n');stop(child)
  if not a.keep_backings:
   (directory/'base.img').unlink()
   if a.backend=='wal':(directory/'journal.img').unlink()
 (out/'status.json').write_text(json.dumps({'returncode':0,'cycles':6,'configurations_per_variant':configs,'repeats_per_variant':3,'backend':a.backend})+'\n')
finally:
 for child in reversed(children):stop(child)
 log.close();trace.close()
