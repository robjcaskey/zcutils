# ZCCUSAN Community Survey Stack

This Terraform stack deploys:

- AWS Lambda (Python) ingester for survey payloads.
- HTTP API Gateway endpoint (`POST /survey`).
- Aurora PostgreSQL Serverless v2 cluster with Data API enabled.
- Data API write path from Lambda with credentials stored in Secrets Manager.

The dependency-free Lambda source and dashboard are packaged directly by the
archive provider. Archive inputs are deterministic, so switching Terraform
between dev and prod state cannot create timestamp-only Lambda drift.

## Files

- `main.tf` – Terraform resources for VPC-backed Aurora + Lambda + API Gateway.
- `variables.tf` – tunables for capacity, names, runtime, and table.
- `outputs.tf` – endpoint names and cluster metadata.
- `lambda/main.py` – handler that persists JSON payloads.
- `lambda/requirements.txt` / `lambda/pyproject.toml` – Python package metadata.

## Deploy dev and prod independently

Each environment has a separate remote state key and a distinct set of AWS
resource names. The wrapper refuses unknown environment names so an accidental
typo cannot silently create another stack:

```bash
cd zccusan/community-survey/terraform
AWS_PROFILE=tf ./deploy-environment.sh dev plan
AWS_PROFILE=tf ./deploy-environment.sh dev apply
AWS_PROFILE=tf ./deploy-environment.sh prod plan
```

The state keys are:

- `zcutils/community-survey/dev/terraform.tfstate`
- `zcutils/community-survey/prod/terraform.tfstate`

The equivalent explicit production commands are:

```bash
cd zccusan/community-survey/terraform
AWS_PROFILE=tf terraform init \
  -backend-config='bucket=caskey-terraform-state-storage' \
  -backend-config='key=zcutils/community-survey/prod/terraform.tfstate' \
  -backend-config='region=us-east-1' \
  -backend-config='profile=tf' \
  -backend-config='encrypt=true' \
  -backend-config='use_lockfile=true'
AWS_PROFILE=tf terraform plan -var-file=production.tfvars
AWS_PROFILE=tf terraform apply -var-file=production.tfvars
```

Production uses remote, encrypted, versioned S3 state; Aurora-managed database
credentials; deletion protection; a required final snapshot; 14-day database
backup retention; 30-day Lambda/API log retention; and API Gateway throttling.
Development uses a smaller maximum Aurora capacity, one-day backup retention,
seven-day log retention, lower public API throttles, and no deletion protection.
Both environments keep Aurora at the service's 0.5 ACU minimum and therefore
incur ongoing database charges while deployed.

After apply, capture:

- `survey_post_url` from output
- `api_endpoint`
- `dashboard_url`

Set these in your community survey publisher:

```bash
export ZCCUSAN_SURVEY_ENABLED=true
export ZCCUSAN_SURVEY_BACKEND_URL="$survey_post_url"
```

If you need to disable reporting without changing app code:

```bash
export ZCCUSAN_SURVEY_ENABLED=false
```

Then post survey payloads:

```bash
curl -s -X POST "$survey_post_url" \
  -H 'content-type: application/json' \
  -d '{"event_type":"community_survey","cloud_region":"us-east-1","answers":{"q1":"yes"}}'
```

Stream multiple events in a single POST using any of these formats:

1) JSON array:
```bash
curl -s -X POST "$survey_post_url" \
  -H 'content-type: application/json' \
  -d '[
    {"event_type":"community_survey","cloud_region":"us-east-1","answers":{"q1":"yes"}},
    {"event_type":"community_survey","cloud_region":"us-west-2","answers":{"q2":"no"}}
  ]'
```

2) Wrapped with `events`:
```bash
curl -s -X POST "$survey_post_url" \
  -H 'content-type: application/json' \
  -d '{"events":[
    {"event_type":"community_survey","cloud_region":"us-east-1","answers":{"q1":"yes"}}
  ]}'
```

3) NDJSON stream in the request body (one JSON object per line).

Success response:
```json
{
  "ok": true,
  "accepted": 2,
  "event_ids": ["...","..."],
  "legacy_event_id": null
}
```

## Notes

- This stack currently uses the AWS account's default VPC subnets.
- Keep `terraform` and AWS credentials scoped to least privilege when running this in CI/CD.
- `survey_post_url` is the default backend endpoint you can pass to `ZCCUSAN_SURVEY_BACKEND_URL`.
- Raw normalized events are retained as append-only JSONB for server-side replay.
  They are not exposed by the public API or dashboard.
- `/community/environments` reads only a sanitized projection containing a
  one-way environment alias and coarse operational aggregates. `/dashboard`
  visualizes active and historical environments from that projection.
