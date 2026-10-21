#!/usr/bin/env python3
"""Bounded exploratory TCP bursts against a disposable RAM leaf on another host.

Start the leaf and echo service separately. This script writes the entire given
capacity; never point it at existing data. Run once from each host for both
client/target directions. Protocol byte budgets exclude TCP/IP and retransmits.
"""
import argparse,hashlib,ipaddress,json,os,pathlib,platform,statistics,subprocess,time

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--probe',required=True)
p.add_argument('--address',required=True);p.add_argument('--echo-address',required=True)
p.add_argument('--local-region',required=True);p.add_argument('--peer-region',required=True)
p.add_argument('--network-path',choices=['private','public'],required=True)
p.add_argument('--direction',choices=['linux-to-mac','mac-to-linux','local-smoke'],required=True)
p.add_argument('--out',required=True,type=pathlib.Path)
p.add_argument('--workers',type=int,default=1);p.add_argument('--ops-per-worker',type=int,default=256)
p.add_argument('--capacity-mib',type=int,default=8);p.add_argument('--budget-mib',type=int,default=64)
p.add_argument('--timeout-seconds',type=float,default=10)
p.add_argument('--max-total-seconds',type=float,default=60)
p.add_argument('--plan-only',action='store_true')
a=p.parse_args()
if a.local_region!=a.peer_region:p.error('peers must be in the same region')
if not 1<=a.workers<=64 or not 1<=a.ops_per_worker<=4096:p.error('workers 1..64 and operations 1..4096 required')
if not 1<=a.capacity_mib<=1024 or a.budget_mib<=0 or not 0<a.timeout_seconds<=60 or not 0<a.max_total_seconds<=300:p.error('invalid capacity, budget, or timeout')
if any(os.environ.get(k)=='1' for k in ['URING_PLAY_TOPOLOGY_STRICT','URING_PLAY_TOPOLOGY_FATAL']):
 p.error('strict topology unavailable: OS-scheduled userspace TCP burst harness')
def endpoint(value):
 try:
  host,port=value.rsplit(':',1);ip=ipaddress.ip_address(host)
  if ip.version!=4 or not 1<=int(port)<=65535:raise ValueError()
  return ip
 except ValueError:p.error('numeric IPv4:port endpoints required')
leaf_ip=endpoint(a.address);echo_ip=endpoint(a.echo_address)
if leaf_ip!=echo_ip:p.error('echo service must be on the leaf host')
if a.network_path=='private' and not leaf_ip.is_private:p.error('private path requires a private IP')
capacity=a.capacity_mib*1024*1024
if capacity%(4096*a.workers):p.error('capacity must divide evenly across 4 KiB worker ranges')
qds=[1,2,4,8,16];repeats=3;io_bytes=4096
# Request + response headers are 128 bytes total per I/O or final SYNC.
prefill_ops=capacity//65536
# Include prefill, all 30 measured invocations, and their per-worker drains.
protocol_bytes=(200+501)*128+prefill_ops*(65536+128)+128
protocol_bytes+=repeats*len(qds)*2*(a.ops_per_worker*a.workers*(io_bytes+128)+a.workers*128)
plan={'protocol_bytes_upper_bound':protocol_bytes,'protocol_budget_bytes':a.budget_mib*1024*1024,
 'budget_scope':'application protocol only; excludes TCP/IP overhead and retransmissions',
 'configuration_count':len(qds)*2,'repeats':repeats,'capacity_bytes':capacity,
 'direction':a.direction,'network_path':a.network_path,'local_region':a.local_region,'peer_region':a.peer_region,
 'workers':a.workers,'per_worker_qds':qds,'ops_per_worker':a.ops_per_worker,
 'per_invocation_timeout_seconds':a.timeout_seconds,'maximum_total_seconds':a.max_total_seconds,'representative':False,
 'topology':'OS-scheduled sender/receiver and target thread per lane; no CPU pinning, hugepages, memlock or io_uring',
 'warning':'Short bounded correctness/latency bursts; insufficient for representative throughput or regression acceptance'}
if protocol_bytes>plan['protocol_budget_bytes']:p.error('planned protocol bytes exceed budget before any connection')
if a.plan_only:print(json.dumps(plan,indent=2));raise SystemExit(0)
binary=pathlib.Path(a.probe).resolve()
if not binary.is_file():p.error('probe binary missing')
a.out.mkdir(parents=True,exist_ok=False)
plan.update(probe_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),platform=platform.platform(),args=vars(a)|{'out':str(a.out)})
(a.out/'metadata.json').write_text(json.dumps(plan,indent=2)+'\n')
print(plan['warning'],flush=True)
started=time.monotonic();used=0
with (a.out/'probe-stderr.log').open('w') as trace:
 def call(*args,cost):
  global used
  if used+cost>plan['protocol_budget_bytes']:raise RuntimeError('protocol budget exhausted')
  remaining=a.max_total_seconds-(time.monotonic()-started)
  if remaining<=0:raise TimeoutError('total burst deadline exhausted')
  used+=cost # Charge the full possible amount even if the invocation fails.
  return json.loads(subprocess.check_output([str(binary),*map(str,args)],text=True,stderr=trace,timeout=min(a.timeout_seconds,remaining)))
 try:
  rtt=call('rtt',a.echo_address,501,cost=(200+501)*128)
  (a.out/'raw-rtt.json').write_text(json.dumps(rtt)+'\n')
  call('run',a.address,'write',prefill_ops,1,16,65536,capacity,rtt['min_us'],'sequential','volatile-ram',cost=prefill_ops*(65536+128)+128)
  rows=[]
  for repeat in range(repeats):
   for qd in qds:
    for operation in ['read-verify','write']:
     row=call('run',a.address,operation,a.ops_per_worker,a.workers,qd,io_bytes,capacity,rtt['min_us'],'random','volatile-ram',cost=a.ops_per_worker*a.workers*(io_bytes+128)+a.workers*128)
     row.update(repeat=repeat,direction=a.direction,network_path=a.network_path,burst_only=True)
     rows.append(row)
     with (a.out/'results.jsonl').open('a') as result:result.write(json.dumps(row)+'\n')
  summary=[]
  for qd in qds:
   for operation in ['read-verify','write']:
    group=[r for r in rows if r['operation']==operation and r['per_worker_qd']==qd]
    rates=[r['iops'] for r in group]
    summary.append({'operation':operation,'workers':a.workers,'lanes':a.workers,'per_worker_qd':qd,'aggregate_depth':a.workers*qd,'iops_min_median_max':[min(rates),statistics.median(rates),max(rates)],'shortest_trial_seconds':min(r['elapsed_s'] for r in group),'raw_tcp_echo_rtt_us':rtt['min_us'],'rtt_iops_ceiling':group[0]['rtt_iops_ceiling'],'efficiency_min_median_max':[min(r['actual_theoretical_efficiency'] for r in group),statistics.median(r['actual_theoretical_efficiency'] for r in group),max(r['actual_theoretical_efficiency'] for r in group)],'completion_semantics':group[0]['completion_semantics'],'representative':False})
  (a.out/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
  status={'returncode':0,'trials':len(rows),'charged_protocol_bytes':used,'elapsed_seconds':time.monotonic()-started}
 except BaseException as error:
  (a.out/'status.json').write_text(json.dumps({'returncode':1,'error':str(error),'charged_protocol_bytes':used})+'\n');raise
 (a.out/'status.json').write_text(json.dumps(status)+'\n')
 print(json.dumps(status))
