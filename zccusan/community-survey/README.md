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
    {"event_type":"community_survey","cloud_region":"us-east-1","payload":{"q1":"yes"}},
    {"event_type":"community_survey","cloud_region":"us-east-1","payload":{"q2":"no"}}
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

Deployed environments:

- Development: `https://g94w93q6w7.execute-api.us-east-1.amazonaws.com`
- Production: `https://vdq4ma9dl2.execute-api.us-east-1.amazonaws.com`

Each has independent API Gateway, Lambda, request-metered Aurora DSQL, and
Terraform state. The Helm telemetry values profiles select the corresponding
`/survey` endpoint. DSQL has no database instances, capacity floor, pause, or
resume delay; idle clusters consume zero DPUs.

## Success response shape

`{"ok":true,"accepted":2,"event_ids":[...],"legacy_event_id":null}`

This uses:
- AWS Lambda (Python runtime)
- HTTP API Gateway route
- Aurora DSQL (PostgreSQL-compatible wire protocol, request-metered DPUs)
- IAM database authentication with short-lived connection tokens

## Environment variable contract (client side)

Programs route events to an enabled, defined management telemetry server before
considering direct survey delivery. Use these environment variables in a
publisher, CLI, or containerized service:

- `ZCCUSAN_MANAGEMENT_CHECKIN_ENABLED`
  Set to `false` to bypass the management telemetry server. Default is `true`.
- `ZCCUSAN_MANAGEMENT_CHECKIN_URL`
  Event-ingestion URL for the management telemetry server, for example
  `http://zccusan-telemetry:9899/v1/events`. When enabled and defined, programs
  send events only to this URL; the server participates in the community survey
  on their behalf.

- `ZCCUSAN_SURVEY_ENABLED`  
  Set to `false` to disable direct community-survey participation when no
  management telemetry server is selected. It does not disable local management
  telemetry delivery. Default is `true`.
- `ZCCUSAN_SURVEY_BACKEND_URL`  
  Community survey endpoint used by the direct fallback and by the telemetry
  server when it has no `ZCCUSAN_TELEMETRY_UPSTREAM_URL` override.

Example:

```bash
export ZCCUSAN_SURVEY_ENABLED=false
export ZCCUSAN_SURVEY_BACKEND_URL="https://example.com/override/survey"
```

The telemetry server itself logs every accepted event to stdout as one-line
NDJSON. When survey participation is enabled, it forwards every event to
`ZCCUSAN_TELEMETRY_UPSTREAM_URL`, falling back to
`ZCCUSAN_SURVEY_BACKEND_URL`. Setting `ZCCUSAN_SURVEY_ENABLED=false` keeps local
ingestion and stdout logging active while disabling its outbound survey sender.

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
overflow event for the community survey, and only then appends the new event.
Inputs larger than 4 KiB receive HTTP 413 when no event
from the request can be accepted; mixed batches report their rejected count.
