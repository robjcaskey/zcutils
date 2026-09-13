#!/usr/bin/env bash
# Read-only raw AWS audit. Includes regions the Spot helper might not enumerate.
set -euo pipefail
[[ $# = 1 ]] || { echo "usage: $0 OUTPUT_DIR" >&2; exit 2; }
out=$1
mkdir -p "$out"
profile=${ADHOC_AWS_PROFILE:-tf}
aws --profile "$profile" --region us-east-1 ec2 describe-regions --all-regions >"$out/regions.json"
audit_region() {
    local region=$1 dir="$out/$1"
    mkdir -p "$dir"
    local aws=(aws --profile "$profile" --region "$region")
    "${aws[@]}" ec2 describe-instances --filters Name=instance-state-name,Values=pending,running,stopping,shutting-down \
        --query 'Reservations[].Instances[].{Id:InstanceId,State:State.Name,Az:Placement.AvailabilityZone,Tags:Tags}' >"$dir/ec2-active.json"
    "${aws[@]}" eks list-clusters --query clusters >"$dir/eks.json"
    "${aws[@]}" autoscaling describe-auto-scaling-groups --query 'AutoScalingGroups[].{Name:AutoScalingGroupName,Desired:DesiredCapacity,Min:MinSize,Max:MaxSize}' >"$dir/asg.json"
    "${aws[@]}" elbv2 describe-load-balancers --query 'LoadBalancers[].LoadBalancerArn' >"$dir/elbv2.json"
    "${aws[@]}" elb describe-load-balancers --query 'LoadBalancerDescriptions[].LoadBalancerName' >"$dir/elb.json"
    "${aws[@]}" ec2 describe-nat-gateways --filter Name=state,Values=pending,available,deleting \
        --query 'NatGateways[].{Id:NatGatewayId,State:State,Vpc:VpcId}' >"$dir/nat.json"
    "${aws[@]}" ec2 describe-addresses --filters 'Name=tag:uringPlayRunId,Values=zc-client-wal-*' --query Addresses >"$dir/task-eip.json"
    "${aws[@]}" ec2 describe-volumes --filters 'Name=tag:uringPlayRunId,Values=zc-client-wal-*' --query 'Volumes[].{Id:VolumeId,State:State}' >"$dir/task-volumes.json"
    "${aws[@]}" ec2 describe-vpcs --filters 'Name=tag:uringPlayRunId,Values=zc-client-wal-*' --query 'Vpcs[].VpcId' >"$dir/task-vpc.json"
    "${aws[@]}" ec2 describe-security-groups --filters 'Name=tag:uringPlayRunId,Values=zc-client-wal-*' --query 'SecurityGroups[].GroupId' >"$dir/task-sg.json"
    "${aws[@]}" ec2 describe-internet-gateways --filters 'Name=tag:uringPlayRunId,Values=zc-client-wal-*' --query 'InternetGateways[].InternetGatewayId' >"$dir/task-igw.json"
    "${aws[@]}" ec2 describe-network-interfaces --filters 'Name=tag:uringPlayRunId,Values=zc-client-wal-*' --query 'NetworkInterfaces[].NetworkInterfaceId' >"$dir/task-eni.json"
    jq -n --arg region "$region" --slurpfile active "$dir/ec2-active.json" \
        --slurpfile eks "$dir/eks.json" --slurpfile asg "$dir/asg.json" \
        --slurpfile elbv2 "$dir/elbv2.json" --slurpfile elb "$dir/elb.json" \
        --slurpfile nat "$dir/nat.json" --slurpfile eip "$dir/task-eip.json" \
        --slurpfile volumes "$dir/task-volumes.json" --slurpfile vpc "$dir/task-vpc.json" \
        --slurpfile sg "$dir/task-sg.json" --slurpfile igw "$dir/task-igw.json" --slurpfile eni "$dir/task-eni.json" \
        '{region:$region,active_ec2:($active[0]|length),eks:($eks[0]|length),asg:($asg[0]|length),elb:($elb[0]|length),elbv2:($elbv2[0]|length),nat:($nat[0]|length),task_eip:($eip[0]|length),task_volumes:($volumes[0]|length),task_vpc:($vpc[0]|length),task_sg:($sg[0]|length),task_igw:($igw[0]|length),task_eni:($eni[0]|length)}' >"$dir/summary.json"
}
pids=()
while read -r region; do
    audit_region "$region" >"$out/$region.log" 2>&1 &
    pids+=("$!")
    if (( ${#pids[@]} == 4 )); then for pid in "${pids[@]}"; do wait "$pid"; done; pids=(); fi
done < <(jq -r '.Regions[] | select(.OptInStatus!="not-opted-in") | .RegionName' "$out/regions.json" | sort)
for pid in "${pids[@]}"; do wait "$pid"; done
jq -s '.' "$out"/*/summary.json >"$out/summary.json"
jq -e 'length>0 and all(.[]; del(.region) | all(.[]; .==0))' "$out/summary.json"
date -u +%FT%TZ >"$out/completed-at"
