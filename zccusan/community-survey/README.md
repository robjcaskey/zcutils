# ZCCUSAN Community Survey (Terraform + Lambda + Aurora DSQL)

Provision this stack with Terraform:

```bash
cd zccusan/community-survey/terraform
terraform init
terraform apply
```

The API endpoint is exposed as `survey_post_url` for `POST` payloads at `/survey`.

```bash
curl -X POST "$SURVEY_POST_URL" \
  -H 'content-type: application/json' \
  -d '{"events":[
    {"telemetry_schema_version":2,"anonymization_schema_version":2,"event_type":"csi_hourly_stats","cloud_provider":"aws","cloud_region":"us-east-1","anonymous_installation_id":"anon-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","active_volume_count":2,"total_iops":1200},
    {"telemetry_schema_version":2,"anonymization_schema_version":2,"event_type":"csi_hourly_stats","cloud_provider":"gcp","cloud_region":"us-central1","anonymous_installation_id":"anon-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","active_volume_count":1,"total_iops":400}
  ]}'
```

A single-event body is still supported.

The production stack also exposes a privacy-filtered community dashboard at
the Terraform `dashboard_url` output. Raw normalized events remain in an
append-only JSONB table so projections can be repaired or replayed later. The
dashboard and its `/community/environments` API never return those raw events,
source-IP hashes, node identifiers, volume identifiers, or private environment
identifiers. Environments appear under stable one-way aliases and are divided
into active and historical views using their last sanitized heartbeat.

Pulse reports `frontend` as the I/O submission boundary, independently of the
network `transport` and the deployment/control plane. `userspace-client` and
`linux-block` distinguish the direct library path from `/dev/zcnblk0`.
Virtual-machine data paths should prefer the concrete label (`qemu-virtio-blk`,
`qemu-virtio-scsi`, `qemu-nvme`, or `vhost-user-blk`); `libvirt-disk` is kept as
a compatibility label when the hypervisor interface is unavailable. Likewise,
`kubernetes-csi` describes a CSI-observed session, not a claim that CSI is on
the I/O hot path. Benchmark records include `io_size_bytes`, so IOPS measured
at different request sizes remain visibly distinct.

Deployed environments:

- Development: `https://g94w93q6w7.execute-api.us-east-1.amazonaws.com`
- Production: `https://vdq4ma9dl2.execute-api.us-east-1.amazonaws.com`

Each has independent API Gateway, Lambda, request-metered Aurora DSQL, and
Terraform state. The Helm community-survey values profiles select the corresponding
`/survey` endpoint. DSQL has no database instances, capacity floor, pause, or
resume delay; idle clusters consume zero DPUs.

## Success response shape

`{"ok":true,"accepted":2,"event_ids":[...],"legacy_event_id":null}`

This uses:
- AWS Lambda (Python runtime)
- HTTP API Gateway route
- Aurora DSQL (PostgreSQL-compatible wire protocol, request-metered DPUs)
- IAM database authentication with short-lived connection tokens

## Endpoint contract

Telemetry and the community survey are separate APIs:

- `ZCCUSAN_TELEMETRY_API_ENDPOINT` is the event-ingestion API selected by an
  operator. When it is set, edge processes send telemetry only there and do not
  directly participate in the community survey. The telemetry service decides
  whether it will participate indirectly.
- `ZCCUSAN_COMMUNITY_SURVEY_API_ENDPOINT` is used only for community-survey
  submission. It never doubles as a telemetry endpoint.
- `ZCCUSAN_COMMUNITY_SURVEY_ENABLED=false` disables direct community
  participation and collector export without disabling local telemetry.

Example:

```bash
export ZCCUSAN_TELEMETRY_API_ENDPOINT="http://zccusan-telemetry:9899/v1/events"
export ZCCUSAN_COMMUNITY_SURVEY_ENABLED=false
```

The telemetry server logs every accepted raw event to stdout as one-line NDJSON.
Before any community HTTPS request leaves the installation, Rust converts each
mixed-version `TelemetryRecord` into the private-field
`NonIdentifyingTelemetry` type. The transformation replaces the installation ID
with a domain-separated SHA-256 alias, retains only explicitly safe aggregate
fields such as cloud provider, region, version, volume count, and IOPS, and
omits node IDs, volume IDs, paths, errors, and unknown fields. The community API
rejects records outside that versioned allowlist and never persists source IPs.

Edge publishers reject events larger than 4 KiB, use a nonblocking in-memory
enqueue, and perform DNS and HTTP work only on a background thread. They abandon
failed delivery eagerly. Explicit publisher shutdown waits no more than 1.5
seconds for a final flush, including DNS failure, a slow acknowledgement, or a
trickle-fed response; telemetry never delays a data-path operation.

The in-cluster telemetry server has different retry semantics. It retains
unacknowledged events in a bounded 4 MiB memory ring and retries non-2xx and
network failures indefinitely. Every accepted event gets a monotonically
increasing `_zccusan_message_index` and is written to stdout before its handler
exits. If the ring fills, the server evicts its oldest unacknowledged records,
logs a `telemetry_buffer_overflow` event containing the exact evicted index
range that had no successful upstream acknowledgement at eviction time, states
that the original NDJSON copies were already emitted to stdout, queues that
anonymized form of that overflow event for the community survey, and only then
appends the new event.
Inputs larger than 4 KiB receive HTTP 413 when no event
from the request can be accepted; mixed batches report their rejected count.
