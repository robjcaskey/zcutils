#!/usr/bin/env python3
"""Compare completed repeated portable probes without claiming representativeness."""
import argparse,json,pathlib,statistics
p=argparse.ArgumentParser();p.add_argument('baseline',type=pathlib.Path);p.add_argument('candidate',type=pathlib.Path);p.add_argument('--output',type=pathlib.Path,required=True);a=p.parse_args()
def load(root):
 groups={}
 for line in (root/'results.jsonl').read_text().splitlines():
  row=json.loads(line);key=(row['io_bytes'],row['workers'],row['per_worker_qd'],row['operation'])
  groups.setdefault(key,[]).append(row)
 meta=json.loads((root/'metadata.json').read_text())
 expected=meta.get('configuration_count',4 if meta['args']['quick'] else 84)
 if len(groups)!=expected or any(len(rows)!=meta['args']['repeats'] for rows in groups.values()):
  raise SystemExit(f'{root}: incomplete matrix; expected {expected} configurations and declared repeat count')
 return groups
base,new=load(a.baseline),load(a.candidate)
if set(base)!=set(new):raise SystemExit('configuration coverage differs; comparison is incomplete')
rows=[]
for key in sorted(base):
 b,c=base[key],new[key]
 if len(b)<3 or len(c)<3:raise SystemExit(f'{key}: fewer than three repeats')
 bi=[r['iops'] for r in b];ci=[r['iops'] for r in c]
 classification='inconclusive_or_within_spread'
 if min(ci)>max(bi)*1.05:classification='candidate_faster_across_observed_spread'
 if max(ci)<min(bi)*.95:classification='candidate_slower_across_observed_spread'
 rows.append({'io_bytes':key[0],'workers':key[1],'per_worker_qd':key[2],'aggregate_depth':key[1]*key[2],'operation':key[3],
  'baseline_iops_min_median_max':[min(bi),statistics.median(bi),max(bi)],'candidate_iops_min_median_max':[min(ci),statistics.median(ci),max(ci)],
  'median_iops_change_percent':100*(statistics.median(ci)/statistics.median(bi)-1),
  'baseline_mean_completion_us_median':statistics.median(r['mean_completion_us'] for r in b),'candidate_mean_completion_us_median':statistics.median(r['mean_completion_us'] for r in c),
  'baseline_echo_reference_us':b[0]['raw_tcp_echo_rtt_us'],'candidate_echo_reference_us':c[0]['raw_tcp_echo_rtt_us'],
  'classification':classification,'representative':False})
a.output.write_text(json.dumps({'warning':'Exploratory shared-system unpinned userspace TCP measurements; timing order/CPU placement may confound differences. Requires alternating-run confirmation before attributing a gain. Not Linux RDMA/kernel-block regression evidence.','configurations':rows},indent=2)+'\n')
print(json.dumps({'configurations':len(rows),'faster_across_spread':sum('faster' in r['classification'] for r in rows),'slower_across_spread':sum('slower' in r['classification'] for r in rows),'output':str(a.output)}))
