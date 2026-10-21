#!/usr/bin/env python3
"""Compare completed repeated portable probes without claiming representativeness."""
import argparse,json,pathlib,statistics
p=argparse.ArgumentParser();p.add_argument('baseline',type=pathlib.Path);p.add_argument('candidate',type=pathlib.Path);p.add_argument('--output',type=pathlib.Path,required=True);p.add_argument('--min-seconds',type=float,default=1.0);a=p.parse_args()
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
def span(values):
 values=[v for v in values if v is not None]
 return [min(values),statistics.median(values),max(values)] if values else None
def completion(row):
 if 'completion_semantics' in row:return row['completion_semantics']
 # Older volatile read/write probes predate the explicit field.
 if row['operation'] in ('read','read-verify'):return 'remote_read_response'
 if row['operation']=='write' and not row['sync_included_in_iops']:return 'remote_write_ack_not_durable'
 raise SystemExit('unknown completion semantics; cannot compare')
def references(group):
 return {'cpu_mappings':sorted(set(r['cpu_mapping'] for r in group)),
  'raw_tcp_echo_rtt_us_min_median_max':span(r['raw_tcp_echo_rtt_us'] for r in group),
  'matching_iops_ceiling_min_median_max':span(r['rtt_iops_ceiling'] for r in group),
  'actual_theoretical_efficiency_min_median_max':span(r['actual_theoretical_efficiency'] for r in group),
  'measured_sync_reference_us_min_median_max':span(r.get('measured_sync_reference_us') or None for r in group)}
rows=[]
for key in sorted(base):
 b,c=base[key],new[key]
 if len(b)<3 or len(c)<3:raise SystemExit(f'{key}: fewer than three repeats')
 semantics={completion(r) for r in b+c};backends={r['backend_semantics'] for r in b+c};patterns={r['pattern'] for r in b+c}
 if len(semantics)!=1 or len(backends)!=1 or len(patterns)!=1:
  raise SystemExit(f'{key}: completion, backing or access pattern differs; comparison rejected')
 bi=[r['iops'] for r in b];ci=[r['iops'] for r in c]
 classification='inconclusive_or_within_spread'
 if min(ci)>max(bi)*1.05:classification='candidate_faster_across_observed_spread'
 if max(ci)<min(bi)*.95:classification='candidate_slower_across_observed_spread'
 shortest=min(r['elapsed_s'] for r in b+c)
 if shortest<a.min_seconds:classification='trials_too_short_for_acceptance'
 if any(r.get('rtt_reference_valid') is False or (r['actual_theoretical_efficiency'] is not None and r['actual_theoretical_efficiency']>1) for r in b+c):classification='invalid_transport_rtt_reference'
 rows.append({'io_bytes':key[0],'workers':key[1],'per_worker_qd':key[2],'aggregate_depth':key[1]*key[2],'lanes':key[1],'operation':key[3],'completion_semantics':next(iter(semantics)),'backend_semantics':next(iter(backends)),'pattern':next(iter(patterns)),
  'baseline_completion_references':references(b),'candidate_completion_references':references(c),
  'baseline_iops_min_median_max':[min(bi),statistics.median(bi),max(bi)],'candidate_iops_min_median_max':[min(ci),statistics.median(ci),max(ci)],
  'shortest_trial_seconds':shortest,'minimum_accepted_trial_seconds':a.min_seconds,'median_iops_change_percent':100*(statistics.median(ci)/statistics.median(bi)-1),
  'baseline_mean_completion_us_median':statistics.median(r['mean_completion_us'] for r in b),'candidate_mean_completion_us_median':statistics.median(r['mean_completion_us'] for r in c),
  'baseline_echo_reference_us':b[0]['raw_tcp_echo_rtt_us'],'candidate_echo_reference_us':c[0]['raw_tcp_echo_rtt_us'],
  'classification':classification,'representative':False})
a.output.write_text(json.dumps({'warning':'Exploratory shared-system unpinned userspace TCP measurements; timing order/CPU placement may confound differences. Requires alternating-run confirmation before attributing a gain. Not Linux RDMA/kernel-block regression evidence.','configurations':rows},indent=2)+'\n')
print(json.dumps({'configurations':len(rows),'faster_across_spread':sum('faster' in r['classification'] for r in rows),'slower_across_spread':sum('slower' in r['classification'] for r in rows),'too_short':sum('too_short' in r['classification'] for r in rows),'invalid_rtt_reference':sum(r['classification']=='invalid_transport_rtt_reference' for r in rows),'output':str(a.output)}))
