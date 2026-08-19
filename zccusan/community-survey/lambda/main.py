import base64
import hashlib
import json
import os
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List

import boto3


RDS_DATA = boto3.client("rds-data")
CLUSTER_ARN = os.environ["SURVEY_DB_CLUSTER_ARN"]
SECRET_ARN = os.environ["SURVEY_DB_SECRET_ARN"]
DATABASE_NAME = os.environ.get("SURVEY_DB_NAME", "community_survey")
TABLE_NAME = os.environ.get("SURVEY_DB_TABLE", "survey_events")
ENVIRONMENTS_TABLE = os.environ.get("SURVEY_ENVIRONMENTS_TABLE", "community_environments")
ACTIVE_WINDOW_HOURS = int(os.environ.get("SURVEY_ACTIVE_WINDOW_HOURS", "2"))
MAX_EVENT_BYTES = 4 * 1024
INSERT_BATCH_SIZE = 64
_DASHBOARD_HTML = (Path(__file__).with_name("dashboard.html").read_text(encoding="utf-8"))

_SCHEMA_READY = False


def _response(status: int, body: Dict[str, Any]) -> Dict[str, Any]:
    return {
        "statusCode": status,
        "isBase64Encoded": False,
        "headers": {"content-type": "application/json"},
        "body": json.dumps(body),
    }


def _html_response(body: str) -> Dict[str, Any]:
    return {
        "statusCode": 200,
        "isBase64Encoded": False,
        "headers": {
            "content-type": "text/html; charset=utf-8",
            "cache-control": "public, max-age=60",
            "x-content-type-options": "nosniff",
            "content-security-policy": (
                "default-src 'self'; style-src 'self' 'unsafe-inline'; "
                "script-src 'self' 'unsafe-inline'; connect-src 'self'; "
                "img-src 'self' data:; frame-ancestors 'none'"
            ),
        },
        "body": body,
    }


def _execute_sql(sql: str, parameters=None):
    return RDS_DATA.execute_statement(
        resourceArn=CLUSTER_ARN,
        secretArn=SECRET_ARN,
        database=DATABASE_NAME,
        sql=sql,
        parameters=parameters or [],
    )


def _query_sql(sql: str, parameters=None) -> List[Dict[str, Any]]:
    result = RDS_DATA.execute_statement(
        resourceArn=CLUSTER_ARN,
        secretArn=SECRET_ARN,
        database=DATABASE_NAME,
        sql=sql,
        parameters=parameters or [],
        formatRecordsAs="JSON",
    )
    formatted = result.get("formattedRecords", "[]")
    return json.loads(formatted)


def _ensure_schema() -> None:
    global _SCHEMA_READY
    if _SCHEMA_READY:
        return

    create_table_sql = f"""
    CREATE TABLE IF NOT EXISTS {TABLE_NAME} (
      id BIGSERIAL PRIMARY KEY,
      event_id TEXT NOT NULL UNIQUE,
      event_type TEXT NOT NULL,
      region TEXT,
      received_at TIMESTAMPTZ NOT NULL,
      source_ip_hash TEXT,
      payload JSONB NOT NULL,
      created_at TIMESTAMPTZ NOT NULL DEFAULT now()
    );
    """
    _execute_sql(create_table_sql)
    _execute_sql(f"""
    CREATE OR REPLACE FUNCTION {TABLE_NAME}_reject_mutation()
    RETURNS trigger AS $$
    BEGIN
      RAISE EXCEPTION 'raw survey events are append-only';
    END;
    $$ LANGUAGE plpgsql;
    """)
    _execute_sql(f"""
    DO $$
    BEGIN
      IF NOT EXISTS (
        SELECT 1 FROM pg_trigger WHERE tgname = '{TABLE_NAME}_immutable'
      ) THEN
        CREATE TRIGGER {TABLE_NAME}_immutable
        BEFORE UPDATE OR DELETE ON {TABLE_NAME}
        FOR EACH ROW EXECUTE FUNCTION {TABLE_NAME}_reject_mutation();
      END IF;
    END
    $$;
    """)
    _execute_sql(f"""
    CREATE TABLE IF NOT EXISTS {ENVIRONMENTS_TABLE} (
      public_environment_id TEXT PRIMARY KEY,
      first_seen TIMESTAMPTZ NOT NULL,
      last_seen TIMESTAMPTZ NOT NULL,
      event_count BIGINT NOT NULL DEFAULT 0,
      region TEXT,
      version TEXT,
      last_event_type TEXT,
      active_volume_count BIGINT,
      recent_iops BIGINT
    );
    """)
    _execute_sql(
        f"CREATE INDEX IF NOT EXISTS {ENVIRONMENTS_TABLE}_last_seen_idx "
        f"ON {ENVIRONMENTS_TABLE} (last_seen DESC);"
    )
    _SCHEMA_READY = True


def _hash_value(value: str) -> str:
    if not value:
        return ""
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def _extract_body_text(event):
    raw_body = event.get("body", "")
    if event.get("isBase64Encoded", False):
        raw_body = base64.b64decode(raw_body).decode("utf-8")
    if raw_body is None:
        return ""
    if isinstance(raw_body, str):
        return raw_body
    return json.dumps(raw_body)


def _normalize_events(parsed_payload: Any) -> List[Dict[str, Any]]:
    if isinstance(parsed_payload, dict):
        if "events" in parsed_payload:
            events = parsed_payload["events"]
            if not isinstance(events, list):
                raise ValueError("events_field_must_be_array")
            return events
        return [parsed_payload]

    if isinstance(parsed_payload, list):
        return parsed_payload

    raise ValueError("payload_must_be_object_or_array")


def _parse_event_stream(body_text: str) -> List[Dict[str, Any]]:
    text = (body_text or "").strip()
    if not text:
        return []

    try:
        parsed = json.loads(text)
        return _normalize_events(parsed)
    except json.JSONDecodeError:
        pass

    events: List[Dict[str, Any]] = []
    for line in body_text.splitlines():
        line = line.strip()
        if not line:
            continue
        events.append(json.loads(line))
    if not events:
        raise json.JSONDecodeError("invalid_json", body_text, 0)
    return _normalize_events(events)


def _event_parameters(
    event_id: str,
    event_type: str,
    region: str,
    source_ip_hash: str,
    payload: Dict[str, Any],
) -> List[Dict[str, Any]]:
    return [
        {"name": "event_id", "value": {"stringValue": event_id}},
        {"name": "event_type", "value": {"stringValue": event_type}},
        {"name": "region", "value": {"stringValue": region} if region else {"isNull": True}},
        {"name": "source_ip_hash", "value": {"stringValue": source_ip_hash} if source_ip_hash else {"isNull": True}},
        {"name": "payload", "value": {"stringValue": json.dumps(payload, separators=(",", ":"))}},
    ]


def _insert_events(parameter_sets: List[List[Dict[str, Any]]]) -> None:
    sql = f"""
        INSERT INTO {TABLE_NAME} (
          event_id, event_type, region, received_at, source_ip_hash, payload
        ) VALUES (
          :event_id, :event_type, :region, now(), :source_ip_hash, :payload::jsonb
        )
        """
    for start in range(0, len(parameter_sets), INSERT_BATCH_SIZE):
        RDS_DATA.batch_execute_statement(
            resourceArn=CLUSTER_ARN,
            secretArn=SECRET_ARN,
            database=DATABASE_NAME,
            sql=sql,
            parameterSets=parameter_sets[start:start + INSERT_BATCH_SIZE],
        )


def _safe_nonnegative_int(value: Any):
    if isinstance(value, bool):
        return None
    try:
        return max(0, int(value))
    except (TypeError, ValueError):
        return None


def _upsert_environment(
    payload_event: Dict[str, Any],
    event_type: str,
    region: str,
    event_increment: int,
) -> None:
    environment_id = payload_event.get("environment_id")
    if not isinstance(environment_id, str) or not environment_id.strip():
        return

    public_id = "env-" + _hash_value(environment_id.strip())[:12]
    version = payload_event.get("version")
    version = version[:64] if isinstance(version, str) and version else None
    active_volumes = _safe_nonnegative_int(payload_event.get("active_volume_count"))
    recent_iops = _safe_nonnegative_int(payload_event.get("total_iops"))
    _execute_sql(
        f"""
        INSERT INTO {ENVIRONMENTS_TABLE} (
          public_environment_id, first_seen, last_seen, event_count, region,
          version, last_event_type, active_volume_count, recent_iops
        ) VALUES (
          :public_id, now(), now(), :event_increment, :region, :version, :event_type,
          :active_volumes, :recent_iops
        )
        ON CONFLICT (public_environment_id) DO UPDATE SET
          last_seen = now(),
          event_count = {ENVIRONMENTS_TABLE}.event_count + :event_increment,
          region = COALESCE(EXCLUDED.region, {ENVIRONMENTS_TABLE}.region),
          version = COALESCE(EXCLUDED.version, {ENVIRONMENTS_TABLE}.version),
          last_event_type = EXCLUDED.last_event_type,
          active_volume_count = COALESCE(EXCLUDED.active_volume_count, {ENVIRONMENTS_TABLE}.active_volume_count),
          recent_iops = COALESCE(EXCLUDED.recent_iops, {ENVIRONMENTS_TABLE}.recent_iops)
        """,
        parameters=[
            {"name": "public_id", "value": {"stringValue": public_id}},
            {"name": "region", "value": {"stringValue": region} if region else {"isNull": True}},
            {"name": "version", "value": {"stringValue": version} if version else {"isNull": True}},
            {"name": "event_type", "value": {"stringValue": event_type}},
            {"name": "event_increment", "value": {"longValue": event_increment}},
            {"name": "active_volumes", "value": {"longValue": active_volumes} if active_volumes is not None else {"isNull": True}},
            {"name": "recent_iops", "value": {"longValue": recent_iops} if recent_iops is not None else {"isNull": True}},
        ],
    )


def _community_environments() -> Dict[str, Any]:
    rows = _query_sql(
        f"""
        SELECT
          public_environment_id,
          (EXTRACT(EPOCH FROM first_seen) * 1000)::bigint AS first_seen_ms,
          (EXTRACT(EPOCH FROM last_seen) * 1000)::bigint AS last_seen_ms,
          event_count,
          region,
          version,
          last_event_type,
          active_volume_count,
          recent_iops,
          (last_seen >= now() - (:active_hours * interval '1 hour')) AS active
        FROM {ENVIRONMENTS_TABLE}
        ORDER BY last_seen DESC
        LIMIT 500
        """,
        [{"name": "active_hours", "value": {"longValue": ACTIVE_WINDOW_HOURS}}],
    )
    active_count = sum(1 for row in rows if row.get("active"))
    return {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "active_window_hours": ACTIVE_WINDOW_HOURS,
        "summary": {
            "total_environments": len(rows),
            "active_environments": active_count,
            "historical_environments": len(rows) - active_count,
            "reported_active_volumes": sum(row.get("active_volume_count") or 0 for row in rows if row.get("active")),
            "reported_recent_iops": sum(row.get("recent_iops") or 0 for row in rows if row.get("active")),
        },
        "environments": rows,
    }


def handler(event, context):
    request_http = event.get("requestContext", {}).get("http", {})
    method = request_http.get("method", "POST").upper()
    path = request_http.get("path", event.get("rawPath", "/survey"))
    if method == "GET" and path in ("/", "/dashboard"):
        return _html_response(_DASHBOARD_HTML)
    if method == "GET" and path == "/community/environments":
        try:
            _ensure_schema()
            response = _response(200, _community_environments())
            response["headers"]["cache-control"] = "public, max-age=30"
            return response
        except Exception as exc:
            print(json.dumps({"level": "error", "event": "dashboard_query_failed", "error_type": type(exc).__name__}))
            return _response(503, {"error": "dashboard_temporarily_unavailable"})

    if method != "POST" or path != "/survey":
        return _response(404, {"error": "not_found"})

    try:
        body_text = _extract_body_text(event)
        body_json = _parse_event_stream(body_text)
    except json.JSONDecodeError:
        return _response(400, {"error": "invalid_json"})
    except ValueError as exc:
        return _response(400, {"error": str(exc)})

    if not body_json:
        return _response(400, {"error": "empty_event_stream"})

    invalid_events = []
    for index, event_item in enumerate(body_json):
        if not isinstance(event_item, dict):
            invalid_events.append({"index": index, "error": "event_must_be_object"})
        elif len(json.dumps(event_item, separators=(",", ":")).encode("utf-8")) > MAX_EVENT_BYTES:
            invalid_events.append({"index": index, "error": "event_exceeds_4096_bytes"})

    if invalid_events:
        return _response(400, {"error": "invalid_event_stream", "invalid_events": invalid_events})

    request = event.get("requestContext", {}).get("http", {})
    source_ip = request.get("sourceIp", "")
    source_ip_hash = _hash_value(source_ip)[:32] if source_ip else None

    request_id = "unknown"
    if context and hasattr(context, "aws_request_id"):
        request_id = context.aws_request_id

    try:
        _ensure_schema()
        event_ids: List[str] = []
        parameter_sets: List[List[Dict[str, Any]]] = []
        environment_updates = {}
        for index, payload_event in enumerate(body_json):
            event_type = str(payload_event.get("event_type", "survey_submission"))
            region = str(payload_event.get("cloud_region", "")) if payload_event.get("cloud_region") else None
            event_id = _hash_value(f"{request_id}:{index}:{datetime.now(timezone.utc).isoformat()}")

            payload = {
                "event_type": event_type,
                "received_at": datetime.now(timezone.utc).isoformat(),
                "fields": payload_event,
            }

            parameter_sets.append(_event_parameters(event_id, event_type, region, source_ip_hash, payload))
            environment_id = payload_event.get("environment_id")
            if isinstance(environment_id, str) and environment_id.strip():
                environment_key = environment_id.strip()
                previous = environment_updates.get(environment_key)
                event_increment = (previous[3] if previous else 0) + 1
                environment_updates[environment_key] = (
                    payload_event,
                    event_type,
                    region,
                    event_increment,
                )
            event_ids.append(event_id)
        _insert_events(parameter_sets)
        for payload_event, event_type, region, event_increment in environment_updates.values():
            try:
                _upsert_environment(payload_event, event_type, region, event_increment)
            except Exception as projection_exc:
                print(json.dumps({
                    "level": "error",
                    "event": "environment_projection_failed",
                    "error_type": type(projection_exc).__name__,
                }))
    except Exception as exc:
        print(json.dumps({
            "level": "error",
            "event": "survey_storage_failed",
            "error_type": type(exc).__name__,
        }))
        return _response(500, {"error": "storage_failed"})

    return _response(200, {
        "ok": True,
        "accepted": len(event_ids),
        "event_ids": event_ids,
        "legacy_event_id": event_ids[0] if len(event_ids) == 1 else None,
    })
