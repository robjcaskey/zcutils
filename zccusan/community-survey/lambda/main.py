import base64
import hashlib
import hmac
import json
import os
import re
import socket
import threading
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List
from urllib.parse import quote

DSQL_ENDPOINT = os.environ["SURVEY_DSQL_ENDPOINT"]
AWS_REGION = os.environ.get("AWS_REGION", "us-east-1")
DATABASE_NAME = os.environ.get("SURVEY_DB_NAME", "postgres")
TABLE_NAME = os.environ.get("SURVEY_DB_TABLE", "survey_events")
ENVIRONMENTS_TABLE = os.environ.get("SURVEY_ENVIRONMENTS_TABLE", "community_environments")
BENCHMARK_RAILS_TABLE = os.environ.get(
    "SURVEY_BENCHMARK_RAILS_TABLE", "community_benchmark_rails"
)
ACTIVE_WINDOW_HOURS = int(os.environ.get("SURVEY_ACTIVE_WINDOW_HOURS", "2"))
MAX_EVENT_BYTES = 4 * 1024
INSERT_BATCH_SIZE = 64
_DASHBOARD_HTML = (Path(__file__).with_name("dashboard.html").read_text(encoding="utf-8"))

_SCHEMA_READY = os.environ.get("SURVEY_SCHEMA_PREPROVISIONED", "false").lower() == "true"
_CONNECTION = None
_CONNECTION_LOCK = threading.Lock()
_NAMED_PARAMETER = re.compile(r"(?<!:):([A-Za-z_][A-Za-z0-9_]*)")
_SAFE_STRING_FIELDS = (
    "event_type", "cloud_provider", "cloud_region", "version", "phase",
    "component", "backend", "status", "latency_scope",
)
_SAFE_INTEGER_FIELDS = (
    "event_at_ms", "started_at_millis", "interval_secs", "active_volume_count",
    "sampled_volume_count", "missing_device_volume_count", "total_iops",
    "avg_iops_per_volume", "cluster_node_count", "size_bytes", "io_size_bytes", "requested_bytes",
    "evicted_first_message_index", "evicted_last_message_index", "evicted_count",
    "hop_count", "path_count", "latency_sample_count", "latency_p50_ns",
    "latency_p95_ns", "latency_p99_ns", "latency_p995_ns", "latency_p999_ns",
    "latency_jitter_p995_ns",
    "lane_count", "worker_count", "numa_node_count", "nic_count",
    "benchmark_rail_index", "benchmark_rail_count", "logical_operations",
    "measurement_duration_ns",
)
_SAFE_BOOLEAN_FIELDS = (
    "ok", "evicted_events_were_logged_to_stdout",
    "upstream_acknowledged_before_eviction",
    "cpu_pinned", "numa_local",
)
_SAFE_INTEGER_MAP_FIELDS = ("iops_distribution", "backend_iops")
_TOPOLOGY_STRING_FIELDS = (
    "kernel_family", "topology_class", "placement_scope", "frontend",
    "virtualization_family", "lane_mapping",
)
_COMMUNITY_ALLOWED_FIELDS = frozenset(
    (
        "telemetry_schema_version", "anonymization_schema_version",
        "anonymous_installation_id", "anonymous_benchmark_run_id",
        "benchmark_result_scope",
    )
    + _SAFE_STRING_FIELDS
    + _SAFE_INTEGER_FIELDS
    + _SAFE_BOOLEAN_FIELDS
    + _SAFE_INTEGER_MAP_FIELDS
    + _TOPOLOGY_STRING_FIELDS
    + ("transport_paths", "topology_hops")
)
_ANONYMOUS_INSTALLATION_ID = re.compile(r"^anon-[0-9a-f]{64}$")
_ANONYMOUS_BENCHMARK_RUN_ID = re.compile(r"^anonrun-[0-9a-f]{64}$")
_APPROVED_KERNEL = re.compile(
    r"^linux-(?:custom|\d+(?:\.\d+){1,3}|\d+(?:\.\d+){1,3}(?:-\d+)+-(?:aws|generic|azure|gcp))$"
)
_TOPOLOGY_CLASSES = frozenset(("direct", "client-leaf", "client-hop-leaf", "multi-hop", "unknown"))
_PLACEMENT_SCOPES = frozenset((
    "same-placement-group", "same-az", "same-region", "cross-region", "unknown",
))
_HOP_ROLES = frozenset(("client-edge", "userspace-hop", "storage-service", "terminal-leaf"))
_TRANSPORTS = frozenset((
    "tcp", "efa-direct", "efa", "rdma", "libfabric-sockets", "unix",
    "shared-memory", "in-process", "unknown",
))
_HOP_INTEGER_FIELDS = frozenset((
    "ordinal", "path_count", "latency_sample_count", "latency_p50_ns", "latency_p95_ns",
    "latency_p99_ns", "latency_p995_ns", "latency_p999_ns", "latency_jitter_p995_ns",
))
_FRONTENDS = frozenset((
    "userspace-client", "linux-block", "kubernetes-csi", "libvirt-disk",
    "qemu-block", "qemu-virtio-blk", "qemu-virtio-scsi", "qemu-nvme",
    "vhost-user-blk", "spdk-bdev", "nvme-of", "unknown",
))
_VIRTUALIZATION_FAMILIES = frozenset((
    "bare-metal", "nitro-vm", "qemu", "firecracker", "xen", "hyper-v",
    "vmware", "container", "unknown",
))
_LANE_MAPPINGS = frozenset((
    "one-lane-per-worker", "shared-workers", "dedicated-with-spares", "unknown",
))
_BENCHMARK_RESULT_SCOPES = frozenset(("rail", "standalone"))


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


def _connect():
    """Return a reusable IAM-authenticated Aurora DSQL connection."""
    global _CONNECTION
    with _CONNECTION_LOCK:
        if _CONNECTION is not None and not _CONNECTION.closed:
            return _CONNECTION

        profile_started = time.monotonic()
        import psycopg
        psycopg_ready = time.monotonic()

        token = _generate_dsql_admin_token()
        token_ready = time.monotonic()
        ipv4_addresses = list(dict.fromkeys(
            address[4][0]
            for address in socket.getaddrinfo(
                DSQL_ENDPOINT,
                5432,
                family=socket.AF_INET,
                type=socket.SOCK_STREAM,
            )
        ))
        dns_ready = time.monotonic()
        last_error = None
        for ipv4_address in ipv4_addresses:
            try:
                _CONNECTION = psycopg.connect(
                    host=DSQL_ENDPOINT,
                    hostaddr=ipv4_address,
                    port=5432,
                    dbname=DATABASE_NAME,
                    user="admin",
                    password=token,
                    sslmode="require",
                    connect_timeout=2,
                    autocommit=True,
                )
                connected = time.monotonic()
                print(json.dumps({
                    "event": "dsql_connection_profile",
                    "psycopg_import_ms": round((psycopg_ready - profile_started) * 1000, 3),
                    "token_sign_ms": round((token_ready - psycopg_ready) * 1000, 3),
                    "dns_ms": round((dns_ready - token_ready) * 1000, 3),
                    "postgres_connect_ms": round((connected - dns_ready) * 1000, 3),
                }))
                return _CONNECTION
            except psycopg.OperationalError as exc:
                last_error = exc
        if last_error is not None:
            raise last_error
        raise RuntimeError("Aurora DSQL endpoint returned no IPv4 addresses")


def _generate_dsql_admin_token(expires_in: int = 900) -> str:
    """Generate the DSQL DbConnectAdmin SigV4 token without loading an AWS SDK."""
    access_key = os.environ["AWS_ACCESS_KEY_ID"]
    secret_key = os.environ["AWS_SECRET_ACCESS_KEY"]
    session_token = os.environ.get("AWS_SESSION_TOKEN")
    now = datetime.now(timezone.utc)
    amz_date = now.strftime("%Y%m%dT%H%M%SZ")
    date_stamp = now.strftime("%Y%m%d")
    scope = f"{date_stamp}/{AWS_REGION}/dsql/aws4_request"

    parameters = {
        "Action": "DbConnectAdmin",
        "X-Amz-Algorithm": "AWS4-HMAC-SHA256",
        "X-Amz-Credential": f"{access_key}/{scope}",
        "X-Amz-Date": amz_date,
        "X-Amz-Expires": str(expires_in),
        "X-Amz-SignedHeaders": "host",
    }
    if session_token:
        parameters["X-Amz-Security-Token"] = session_token

    canonical_query = "&".join(
        f"{quote(key, safe='-_.~')}={quote(value, safe='-_.~')}"
        for key, value in sorted(parameters.items())
    )
    canonical_request = "\n".join((
        "GET",
        "/",
        canonical_query,
        f"host:{DSQL_ENDPOINT}\n",
        "host",
        hashlib.sha256(b"").hexdigest(),
    ))
    string_to_sign = "\n".join((
        "AWS4-HMAC-SHA256",
        amz_date,
        scope,
        hashlib.sha256(canonical_request.encode("utf-8")).hexdigest(),
    ))

    def sign(key: bytes, value: str) -> bytes:
        return hmac.new(key, value.encode("utf-8"), hashlib.sha256).digest()

    signing_key = sign(
        sign(sign(sign(("AWS4" + secret_key).encode("utf-8"), date_stamp), AWS_REGION), "dsql"),
        "aws4_request",
    )
    signature = hmac.new(
        signing_key,
        string_to_sign.encode("utf-8"),
        hashlib.sha256,
    ).hexdigest()
    return f"{DSQL_ENDPOINT}/?{canonical_query}&X-Amz-Signature={signature}"


def _parameter_values(parameters) -> Dict[str, Any]:
    values = {}
    for parameter in parameters or []:
        value = parameter["value"]
        if value.get("isNull"):
            values[parameter["name"]] = None
        else:
            values[parameter["name"]] = next(iter(value.values()))
    return values


def _psycopg_sql(sql: str) -> str:
    return _NAMED_PARAMETER.sub(r"%(\1)s", sql)


def _run_sql(sql: str, parameters=None, *, fetch=False):
    """Run one DSQL transaction, retrying optimistic concurrency conflicts."""
    global _CONNECTION
    values = _parameter_values(parameters)
    for attempt in range(4):
        try:
            connection = _connect()
            with connection.cursor() as cursor:
                cursor.execute(_psycopg_sql(sql), values)
                if not fetch:
                    return None
                columns = [column.name for column in cursor.description]
                return [dict(zip(columns, row)) for row in cursor.fetchall()]
        except Exception as exc:
            sqlstate = getattr(exc, "sqlstate", None)
            connection_failed = getattr(_CONNECTION, "closed", True)
            if connection_failed:
                _CONNECTION = None
            retryable = connection_failed or sqlstate in ("40001", "08000", "08001", "08006")
            if attempt == 3 or not retryable:
                raise
            time.sleep(0.01 * (2 ** attempt))
    raise RuntimeError("unreachable DSQL retry state")


def _execute_sql(sql: str, parameters=None):
    return _run_sql(sql, parameters)


def _query_sql(sql: str, parameters=None) -> List[Dict[str, Any]]:
    return _run_sql(sql, parameters, fetch=True)


def _ensure_schema() -> None:
    global _SCHEMA_READY
    if _SCHEMA_READY:
        return

    create_table_sql = f"""
    CREATE TABLE IF NOT EXISTS {TABLE_NAME} (
      event_id TEXT PRIMARY KEY,
      event_type TEXT NOT NULL,
      region TEXT,
      received_at TIMESTAMPTZ NOT NULL,
      payload JSONB NOT NULL,
      created_at TIMESTAMPTZ NOT NULL DEFAULT now()
    );
    """
    _execute_sql(create_table_sql)
    _execute_sql(f"""
    CREATE TABLE IF NOT EXISTS {ENVIRONMENTS_TABLE} (
      public_environment_id TEXT PRIMARY KEY,
      first_seen TIMESTAMPTZ NOT NULL,
      last_seen TIMESTAMPTZ NOT NULL,
      event_count BIGINT NOT NULL DEFAULT 0,
      cloud_provider TEXT,
      region TEXT,
      version TEXT,
      last_event_type TEXT,
      active_volume_count BIGINT,
      lifecycle_active BOOLEAN,
      recent_iops BIGINT,
      io_size_bytes BIGINT,
      kernel_family TEXT,
      topology_class TEXT,
      placement_scope TEXT,
      transport_paths JSONB,
      topology_hops JSONB,
      latency_sample_count BIGINT,
      latency_p50_ns BIGINT,
      latency_p99_ns BIGINT,
      latency_p995_ns BIGINT,
      latency_p999_ns BIGINT,
      latency_jitter_p995_ns BIGINT,
      frontend TEXT,
      virtualization_family TEXT,
      lane_count BIGINT,
      worker_count BIGINT,
      nic_count BIGINT,
      numa_node_count BIGINT,
      lane_mapping TEXT,
      cpu_pinned BOOLEAN,
      numa_local BOOLEAN,
      benchmark_run_id TEXT,
      benchmark_status TEXT,
      benchmark_expected_rails BIGINT,
      benchmark_completed_rails BIGINT,
      benchmark_logical_operations BIGINT,
      benchmark_duration_ns BIGINT,
      benchmark_rails JSONB
    );
    """)
    _execute_sql(f"""
    CREATE TABLE IF NOT EXISTS {BENCHMARK_RAILS_TABLE} (
      public_environment_id TEXT NOT NULL,
      benchmark_run_id TEXT NOT NULL,
      rail_index BIGINT NOT NULL,
      expected_rail_count BIGINT NOT NULL,
      logical_operations BIGINT NOT NULL,
      measurement_duration_ns BIGINT NOT NULL,
      reported_iops BIGINT NOT NULL,
      size_bytes BIGINT,
      latency_sample_count BIGINT,
      latency_p50_ns BIGINT,
      latency_p99_ns BIGINT,
      latency_p995_ns BIGINT,
      latency_p999_ns BIGINT,
      latency_jitter_p995_ns BIGINT,
      received_at TIMESTAMPTZ NOT NULL DEFAULT now(),
      PRIMARY KEY (public_environment_id, benchmark_run_id, rail_index)
    );
    """)
    _execute_sql(f"""
    ALTER TABLE {ENVIRONMENTS_TABLE}
      ADD COLUMN IF NOT EXISTS cloud_provider TEXT;
    """)
    _execute_sql(f"""
    ALTER TABLE {ENVIRONMENTS_TABLE}
      ADD COLUMN IF NOT EXISTS lifecycle_active BOOLEAN;
    """)
    for column, sql_type in (
        ("kernel_family", "TEXT"),
        ("io_size_bytes", "BIGINT"),
        ("topology_class", "TEXT"),
        ("placement_scope", "TEXT"),
        ("transport_paths", "JSONB"),
        ("topology_hops", "JSONB"),
        ("latency_sample_count", "BIGINT"),
        ("latency_p50_ns", "BIGINT"),
        ("latency_p99_ns", "BIGINT"),
        ("latency_p995_ns", "BIGINT"),
        ("latency_p999_ns", "BIGINT"),
        ("latency_jitter_p995_ns", "BIGINT"),
        ("frontend", "TEXT"),
        ("virtualization_family", "TEXT"),
        ("lane_count", "BIGINT"),
        ("worker_count", "BIGINT"),
        ("nic_count", "BIGINT"),
        ("numa_node_count", "BIGINT"),
        ("lane_mapping", "TEXT"),
        ("cpu_pinned", "BOOLEAN"),
        ("numa_local", "BOOLEAN"),
        ("benchmark_run_id", "TEXT"),
        ("benchmark_status", "TEXT"),
        ("benchmark_expected_rails", "BIGINT"),
        ("benchmark_completed_rails", "BIGINT"),
        ("benchmark_logical_operations", "BIGINT"),
        ("benchmark_duration_ns", "BIGINT"),
        ("benchmark_rails", "JSONB"),
    ):
        _execute_sql(f"""
        ALTER TABLE {ENVIRONMENTS_TABLE}
          ADD COLUMN IF NOT EXISTS {column} {sql_type};
        """)
    # 1.0.0 was briefly emitted by incorrect package metadata before any 1.x
    # compatibility promise existed. Correct only this known bad literal in
    # the public projection; append-only raw events remain untouched.
    _execute_sql(f"""
    UPDATE {ENVIRONMENTS_TABLE}
       SET version = '0.1.2'
     WHERE version = '1.0.0';
    """)
    # Backfill terminal one-shot operations written by clients predating the
    # explicit active-volume field. Their results remain queryable, but they
    # must not linger in Active until the freshness timeout expires.
    _execute_sql(f"""
    UPDATE {ENVIRONMENTS_TABLE}
       SET lifecycle_active = FALSE,
           active_volume_count = COALESCE(active_volume_count, 0)
     WHERE lifecycle_active IS NULL
       AND last_event_type IN (
         'block_benchmark_result',
         'volume_live_operation_result',
         'raw_volume_snapshot'
       );
    """)
    _SCHEMA_READY = True


def _hash_value(value: str) -> str:
    if not value:
        return ""
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def _bounded_string(value: Any, maximum: int = 128):
    if not isinstance(value, str) or not value.strip():
        return None
    return value.strip()[:maximum]


def _validate_non_identifying_telemetry(event: Dict[str, Any]):
    """Reject anything outside the versioned community-safe wire schema."""
    if event.get("anonymization_schema_version") not in (1, 2):
        return "unsupported_anonymization_schema_version"
    telemetry_schema_version = event.get("telemetry_schema_version")
    if (
        isinstance(telemetry_schema_version, bool)
        or not isinstance(telemetry_schema_version, int)
        or telemetry_schema_version < 0
    ):
        return "invalid_telemetry_schema_version"
    unknown_fields = sorted(set(event) - _COMMUNITY_ALLOWED_FIELDS)
    if unknown_fields:
        # Do not reflect a potentially identifying field name back through logs
        # or intermediary error capture.
        return "fields_not_permitted"

    anonymous_id = event.get("anonymous_installation_id")
    if anonymous_id is not None and (
        not isinstance(anonymous_id, str)
        or not _ANONYMOUS_INSTALLATION_ID.fullmatch(anonymous_id)
    ):
        return "invalid_anonymous_installation_id"
    anonymous_run_id = event.get("anonymous_benchmark_run_id")
    if anonymous_run_id is not None and (
        not isinstance(anonymous_run_id, str)
        or not _ANONYMOUS_BENCHMARK_RUN_ID.fullmatch(anonymous_run_id)
    ):
        return "invalid_anonymous_benchmark_run_id"
    for name in _SAFE_STRING_FIELDS:
        value = event.get(name)
        if value is not None and (
            not isinstance(value, str) or not value.strip() or len(value) > 128
        ):
            return f"invalid_{name}"
    for name in _SAFE_INTEGER_FIELDS:
        value = event.get(name)
        if value is not None and (
            isinstance(value, bool) or not isinstance(value, int) or value < 0
        ):
            return f"invalid_{name}"
    for name in _SAFE_BOOLEAN_FIELDS:
        value = event.get(name)
        if value is not None and not isinstance(value, bool):
            return f"invalid_{name}"
    for name in _SAFE_INTEGER_MAP_FIELDS:
        value = event.get(name)
        if value is None:
            continue
        if not isinstance(value, dict) or len(value) > 64:
            return f"invalid_{name}"
        if any(
            not isinstance(key, str)
            or not key
            or len(key) > 64
            or any(not (character.isalnum() or character in "_-") for character in key)
            or isinstance(item, bool)
            or not isinstance(item, int)
            or item < 0
            for key, item in value.items()
        ):
            return f"invalid_{name}"
    kernel_family = event.get("kernel_family")
    if kernel_family is not None and (
        not isinstance(kernel_family, str) or not _APPROVED_KERNEL.fullmatch(kernel_family)
    ):
        return "invalid_kernel_family"
    topology_class = event.get("topology_class")
    if topology_class is not None and topology_class not in _TOPOLOGY_CLASSES:
        return "invalid_topology_class"
    placement_scope = event.get("placement_scope")
    if placement_scope is not None and placement_scope not in _PLACEMENT_SCOPES:
        return "invalid_placement_scope"
    frontend = event.get("frontend")
    if frontend is not None and frontend not in _FRONTENDS:
        return "invalid_frontend"
    virtualization_family = event.get("virtualization_family")
    if virtualization_family is not None and virtualization_family not in _VIRTUALIZATION_FAMILIES:
        return "invalid_virtualization_family"
    lane_mapping = event.get("lane_mapping")
    if lane_mapping is not None and lane_mapping not in _LANE_MAPPINGS:
        return "invalid_lane_mapping"
    benchmark_scope = event.get("benchmark_result_scope")
    if benchmark_scope is not None and benchmark_scope not in _BENCHMARK_RESULT_SCOPES:
        return "invalid_benchmark_result_scope"
    rail_index = event.get("benchmark_rail_index")
    rail_count = event.get("benchmark_rail_count")
    rail_relationship = (anonymous_run_id, rail_index, rail_count)
    if any(value is not None for value in rail_relationship):
        if anonymous_id is None:
            return "benchmark_rail_requires_installation"
        if any(value is None for value in rail_relationship):
            return "incomplete_benchmark_rail_relationship"
        if event.get("event_type") != "transport_benchmark_result" or benchmark_scope != "rail":
            return "invalid_benchmark_rail_context"
        if not 1 <= rail_count <= 256 or not 0 <= rail_index < rail_count:
            return "invalid_benchmark_rail_ordinal"
        operations = event.get("logical_operations")
        duration_ns = event.get("measurement_duration_ns")
        reported_iops = event.get("total_iops")
        if not operations or not duration_ns or reported_iops is None:
            return "incomplete_benchmark_measurement"
        calculated_iops = (operations * 1_000_000_000 + duration_ns // 2) // duration_ns
        if abs(reported_iops - calculated_iops) > 1:
            return "inconsistent_benchmark_iops"
    elif benchmark_scope == "rail":
        return "incomplete_benchmark_rail_relationship"
    transport_paths = event.get("transport_paths")
    if transport_paths is not None and (
        not isinstance(transport_paths, dict)
        or not transport_paths
        or len(transport_paths) > 16
        or any(
            transport not in _TRANSPORTS
            or isinstance(count, bool)
            or not isinstance(count, int)
            or not 1 <= count <= 256
            for transport, count in transport_paths.items()
        )
    ):
        return "invalid_transport_paths"
    topology_hops = event.get("topology_hops")
    if topology_hops is not None:
        if not isinstance(topology_hops, list) or not 1 <= len(topology_hops) <= 8:
            return "invalid_topology_hops"
        allowed_hop_fields = _HOP_INTEGER_FIELDS | {
            "role", "transport", "kernel_family",
        }
        for ordinal, hop in enumerate(topology_hops):
            if not isinstance(hop, dict) or set(hop) - allowed_hop_fields:
                return "invalid_topology_hops"
            if hop.get("role") not in _HOP_ROLES or hop.get("transport") not in _TRANSPORTS:
                return "invalid_topology_hops"
            if hop.get("ordinal") != ordinal:
                return "invalid_topology_hops"
            if "kernel_family" in hop and (
                not isinstance(hop["kernel_family"], str)
                or not _APPROVED_KERNEL.fullmatch(hop["kernel_family"])
            ):
                return "invalid_topology_hops"
            if any(
                isinstance(value, bool) or not isinstance(value, int) or value < 0
                for name, value in hop.items()
                if name in _HOP_INTEGER_FIELDS
            ):
                return "invalid_topology_hops"
            path_count = hop.get("path_count")
            if path_count is not None and not 1 <= path_count <= 256:
                return "invalid_topology_hops"
    return None


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
    payload: Dict[str, Any],
) -> List[Dict[str, Any]]:
    return [
        {"name": "event_id", "value": {"stringValue": event_id}},
        {"name": "event_type", "value": {"stringValue": event_type}},
        {"name": "region", "value": {"stringValue": region} if region else {"isNull": True}},
        {"name": "payload", "value": {"stringValue": json.dumps(payload, separators=(",", ":"))}},
    ]


def _insert_events(parameter_sets: List[List[Dict[str, Any]]]) -> None:
    global _CONNECTION
    sql = f"""
        INSERT INTO {TABLE_NAME} (
          event_id, event_type, region, received_at, payload
        ) VALUES (
          :event_id, :event_type, :region, now(), :payload::jsonb
        )
        """
    statement = _psycopg_sql(sql)
    for start in range(0, len(parameter_sets), INSERT_BATCH_SIZE):
        batch = [
            _parameter_values(parameters)
            for parameters in parameter_sets[start:start + INSERT_BATCH_SIZE]
        ]
        for attempt in range(4):
            try:
                connection = _connect()
                with connection.transaction():
                    with connection.cursor() as cursor:
                        cursor.executemany(statement, batch)
                break
            except Exception as exc:
                sqlstate = getattr(exc, "sqlstate", None)
                connection_failed = getattr(_CONNECTION, "closed", True)
                if connection_failed:
                    _CONNECTION = None
                retryable = connection_failed or sqlstate in ("40001", "08000", "08001", "08006")
                if attempt == 3 or not retryable:
                    raise
                time.sleep(0.01 * (2 ** attempt))


def _safe_nonnegative_int(value: Any):
    if isinstance(value, bool):
        return None
    try:
        return max(0, int(value))
    except (TypeError, ValueError):
        return None


def _public_environment_id(anonymous_installation_id: str) -> str:
    return "env-" + _hash_value(anonymous_installation_id.strip())[:12]


def _record_benchmark_rail(payload_event: Dict[str, Any]):
    """Persist one rail and derive the parent run from exact integer evidence."""
    if payload_event.get("benchmark_result_scope") != "rail":
        return None
    anonymous_installation_id = payload_event.get("anonymous_installation_id")
    anonymous_run_id = payload_event.get("anonymous_benchmark_run_id")
    if not isinstance(anonymous_installation_id, str) or not isinstance(anonymous_run_id, str):
        return None

    public_id = _public_environment_id(anonymous_installation_id)
    public_run_id = "run-" + _hash_value(anonymous_run_id)[:16]
    rail_index = int(payload_event["benchmark_rail_index"])
    rail_count = int(payload_event["benchmark_rail_count"])
    operations = int(payload_event["logical_operations"])
    duration_ns = int(payload_event["measurement_duration_ns"])
    reported_iops = int(payload_event["total_iops"])

    nullable_integers = {
        "size_bytes": _safe_nonnegative_int(payload_event.get("size_bytes")),
        "latency_sample_count": _safe_nonnegative_int(payload_event.get("latency_sample_count")),
        "latency_p50_ns": _safe_nonnegative_int(payload_event.get("latency_p50_ns")),
        "latency_p99_ns": _safe_nonnegative_int(payload_event.get("latency_p99_ns")),
        "latency_p995_ns": _safe_nonnegative_int(payload_event.get("latency_p995_ns")),
        "latency_p999_ns": _safe_nonnegative_int(payload_event.get("latency_p999_ns")),
        "latency_jitter_p995_ns": _safe_nonnegative_int(payload_event.get("latency_jitter_p995_ns")),
    }
    parameters = [
        {"name": "public_id", "value": {"stringValue": public_id}},
        {"name": "run_id", "value": {"stringValue": public_run_id}},
        {"name": "rail_index", "value": {"longValue": rail_index}},
        {"name": "rail_count", "value": {"longValue": rail_count}},
        {"name": "operations", "value": {"longValue": operations}},
        {"name": "duration_ns", "value": {"longValue": duration_ns}},
        {"name": "reported_iops", "value": {"longValue": reported_iops}},
    ]
    parameters.extend(
        {
            "name": name,
            "value": {"longValue": value} if value is not None else {"isNull": True},
        }
        for name, value in nullable_integers.items()
    )
    _execute_sql(
        f"""
        INSERT INTO {BENCHMARK_RAILS_TABLE} (
          public_environment_id, benchmark_run_id, rail_index, expected_rail_count,
          logical_operations, measurement_duration_ns, reported_iops, size_bytes,
          latency_sample_count, latency_p50_ns, latency_p99_ns, latency_p995_ns,
          latency_p999_ns, latency_jitter_p995_ns, received_at
        ) VALUES (
          :public_id, :run_id, :rail_index, :rail_count, :operations, :duration_ns,
          :reported_iops, :size_bytes, :latency_sample_count, :latency_p50_ns,
          :latency_p99_ns, :latency_p995_ns, :latency_p999_ns,
          :latency_jitter_p995_ns, now()
        )
        ON CONFLICT (public_environment_id, benchmark_run_id, rail_index) DO UPDATE SET
          expected_rail_count = EXCLUDED.expected_rail_count,
          logical_operations = EXCLUDED.logical_operations,
          measurement_duration_ns = EXCLUDED.measurement_duration_ns,
          reported_iops = EXCLUDED.reported_iops,
          size_bytes = EXCLUDED.size_bytes,
          latency_sample_count = EXCLUDED.latency_sample_count,
          latency_p50_ns = EXCLUDED.latency_p50_ns,
          latency_p99_ns = EXCLUDED.latency_p99_ns,
          latency_p995_ns = EXCLUDED.latency_p995_ns,
          latency_p999_ns = EXCLUDED.latency_p999_ns,
          latency_jitter_p995_ns = EXCLUDED.latency_jitter_p995_ns,
          received_at = now()
        """,
        parameters,
    )
    rows = _query_sql(
        f"""
        SELECT rail_index, expected_rail_count, logical_operations,
               measurement_duration_ns, reported_iops, size_bytes,
               latency_sample_count, latency_p50_ns, latency_p99_ns,
               latency_p995_ns, latency_p999_ns, latency_jitter_p995_ns
          FROM {BENCHMARK_RAILS_TABLE}
         WHERE public_environment_id = :public_id
           AND benchmark_run_id = :run_id
         ORDER BY rail_index
        """,
        [
            {"name": "public_id", "value": {"stringValue": public_id}},
            {"name": "run_id", "value": {"stringValue": public_run_id}},
        ],
    )
    usable_rows = [
        row for row in rows
        if _safe_nonnegative_int(row.get("expected_rail_count")) == rail_count
        and _safe_nonnegative_int(row.get("rail_index")) is not None
        and _safe_nonnegative_int(row.get("logical_operations")) is not None
        and (_safe_nonnegative_int(row.get("measurement_duration_ns")) or 0) > 0
    ]
    ordinals = {_safe_nonnegative_int(row.get("rail_index")) for row in usable_rows}
    complete = ordinals == set(range(rail_count))
    rails = []
    for row in usable_rows:
        rail_operations = int(row["logical_operations"])
        rail_duration = int(row["measurement_duration_ns"])
        rail = {
            "rail_index": int(row["rail_index"]),
            "logical_operations": rail_operations,
            "duration_ns": rail_duration,
            "logical_iops": (rail_operations * 1_000_000_000 + rail_duration // 2) // rail_duration,
        }
        for name in (
            "size_bytes", "latency_sample_count", "latency_p50_ns", "latency_p99_ns",
            "latency_p995_ns", "latency_p999_ns", "latency_jitter_p995_ns",
        ):
            value = _safe_nonnegative_int(row.get(name))
            if value is not None:
                rail[name] = value
        rails.append(rail)
    rails.sort(key=lambda rail: rail["rail_index"])
    state = {
        "run_id": public_run_id,
        "status": "complete" if complete else "collecting",
        "expected_rails": rail_count,
        "completed_rails": len(ordinals),
        "rails": rails,
        "logical_operations": None,
        "duration_ns": None,
        "logical_iops": None,
    }
    if complete:
        aggregate_operations = sum(rail["logical_operations"] for rail in rails)
        aggregate_duration_ns = max(rail["duration_ns"] for rail in rails)
        state.update({
            "logical_operations": aggregate_operations,
            "duration_ns": aggregate_duration_ns,
            "logical_iops": (
                aggregate_operations * 1_000_000_000 + aggregate_duration_ns // 2
            ) // aggregate_duration_ns,
        })
    return state


def _upsert_environment(
    payload_event: Dict[str, Any],
    event_type: str,
    region: str,
    event_increment: int,
    benchmark_state=None,
) -> None:
    anonymous_installation_id = payload_event.get("anonymous_installation_id")
    if not isinstance(anonymous_installation_id, str) or not anonymous_installation_id.strip():
        return

    public_id = _public_environment_id(anonymous_installation_id)
    cloud_provider = _bounded_string(payload_event.get("cloud_provider"), 32)
    version = payload_event.get("version")
    version = version[:64] if isinstance(version, str) and version else None
    active_volumes = _safe_nonnegative_int(payload_event.get("active_volume_count"))
    recent_iops = _safe_nonnegative_int(payload_event.get("total_iops"))
    if payload_event.get("benchmark_result_scope") == "rail":
        # A rail is not the benchmark. Publish only the exact parent aggregate,
        # and leave an earlier complete headline untouched while collecting.
        recent_iops = benchmark_state.get("logical_iops") if benchmark_state else None
    io_size_bytes = _safe_nonnegative_int(payload_event.get("io_size_bytes"))
    kernel_family = _bounded_string(payload_event.get("kernel_family"), 96)
    topology_class = _bounded_string(payload_event.get("topology_class"), 32)
    placement_scope = _bounded_string(payload_event.get("placement_scope"), 32)
    frontend = _bounded_string(payload_event.get("frontend"), 32)
    virtualization_family = _bounded_string(payload_event.get("virtualization_family"), 32)
    lane_mapping = _bounded_string(payload_event.get("lane_mapping"), 32)
    transport_paths = payload_event.get("transport_paths")
    topology_hops = payload_event.get("topology_hops")
    transport_paths_json = json.dumps(transport_paths, separators=(",", ":")) if transport_paths else None
    topology_hops_json = json.dumps(topology_hops, separators=(",", ":")) if topology_hops else None
    lane_count = _safe_nonnegative_int(payload_event.get("lane_count"))
    worker_count = _safe_nonnegative_int(payload_event.get("worker_count"))
    nic_count = _safe_nonnegative_int(payload_event.get("nic_count"))
    numa_node_count = _safe_nonnegative_int(payload_event.get("numa_node_count"))
    latency_sample_count = _safe_nonnegative_int(payload_event.get("latency_sample_count"))
    latency_p50_ns = _safe_nonnegative_int(payload_event.get("latency_p50_ns"))
    latency_p99_ns = _safe_nonnegative_int(payload_event.get("latency_p99_ns"))
    latency_p995_ns = _safe_nonnegative_int(payload_event.get("latency_p995_ns"))
    latency_p999_ns = _safe_nonnegative_int(payload_event.get("latency_p999_ns"))
    latency_jitter_p995_ns = _safe_nonnegative_int(payload_event.get("latency_jitter_p995_ns"))
    cpu_pinned = payload_event.get("cpu_pinned") if isinstance(payload_event.get("cpu_pinned"), bool) else None
    numa_local = payload_event.get("numa_local") if isinstance(payload_event.get("numa_local"), bool) else None
    benchmark_run_id = benchmark_state.get("run_id") if benchmark_state else None
    benchmark_status = benchmark_state.get("status") if benchmark_state else None
    benchmark_expected_rails = benchmark_state.get("expected_rails") if benchmark_state else None
    benchmark_completed_rails = benchmark_state.get("completed_rails") if benchmark_state else None
    benchmark_logical_operations = benchmark_state.get("logical_operations") if benchmark_state else None
    benchmark_duration_ns = benchmark_state.get("duration_ns") if benchmark_state else None
    benchmark_rails_json = (
        json.dumps(benchmark_state.get("rails"), separators=(",", ":"))
        if benchmark_state else None
    )
    _execute_sql(
        f"""
        INSERT INTO {ENVIRONMENTS_TABLE} (
          public_environment_id, first_seen, last_seen, event_count, cloud_provider, region,
          version, last_event_type, active_volume_count, lifecycle_active, recent_iops, io_size_bytes,
          kernel_family, topology_class, placement_scope, transport_paths, topology_hops,
          latency_sample_count, latency_p50_ns, latency_p99_ns, latency_p995_ns,
          latency_p999_ns, latency_jitter_p995_ns, frontend, virtualization_family,
          lane_count, worker_count, nic_count, numa_node_count, lane_mapping, cpu_pinned, numa_local,
          benchmark_run_id, benchmark_status, benchmark_expected_rails,
          benchmark_completed_rails, benchmark_logical_operations,
          benchmark_duration_ns, benchmark_rails
        ) VALUES (
          :public_id, now(), now(), :event_increment, :cloud_provider, :region, :version, :event_type,
          :active_volumes,
          CASE WHEN :active_volumes IS NULL THEN NULL ELSE :active_volumes > 0 END,
          :recent_iops, :io_size_bytes, :kernel_family, :topology_class, :placement_scope,
          :transport_paths::jsonb, :topology_hops::jsonb, :latency_sample_count,
          :latency_p50_ns, :latency_p99_ns, :latency_p995_ns, :latency_p999_ns,
          :latency_jitter_p995_ns, :frontend, :virtualization_family, :lane_count,
          :worker_count, :nic_count, :numa_node_count, :lane_mapping, :cpu_pinned, :numa_local,
          :benchmark_run_id, :benchmark_status, :benchmark_expected_rails,
          :benchmark_completed_rails, :benchmark_logical_operations,
          :benchmark_duration_ns, :benchmark_rails::jsonb
        )
        ON CONFLICT (public_environment_id) DO UPDATE SET
          last_seen = now(),
          event_count = {ENVIRONMENTS_TABLE}.event_count + :event_increment,
          cloud_provider = COALESCE(EXCLUDED.cloud_provider, {ENVIRONMENTS_TABLE}.cloud_provider),
          region = COALESCE(EXCLUDED.region, {ENVIRONMENTS_TABLE}.region),
          version = COALESCE(EXCLUDED.version, {ENVIRONMENTS_TABLE}.version),
          last_event_type = EXCLUDED.last_event_type,
          active_volume_count = COALESCE(EXCLUDED.active_volume_count, {ENVIRONMENTS_TABLE}.active_volume_count),
          lifecycle_active = COALESCE(EXCLUDED.lifecycle_active, {ENVIRONMENTS_TABLE}.lifecycle_active),
          recent_iops = CASE
            WHEN :benchmark_present
             AND {ENVIRONMENTS_TABLE}.benchmark_run_id = EXCLUDED.benchmark_run_id
             AND {ENVIRONMENTS_TABLE}.benchmark_status = 'complete'
             AND EXCLUDED.benchmark_status = 'collecting'
              THEN {ENVIRONMENTS_TABLE}.recent_iops
            WHEN :benchmark_present THEN EXCLUDED.recent_iops
            ELSE COALESCE(EXCLUDED.recent_iops, {ENVIRONMENTS_TABLE}.recent_iops)
          END,
          io_size_bytes = COALESCE(EXCLUDED.io_size_bytes, {ENVIRONMENTS_TABLE}.io_size_bytes),
          kernel_family = COALESCE(EXCLUDED.kernel_family, {ENVIRONMENTS_TABLE}.kernel_family),
          topology_class = COALESCE(EXCLUDED.topology_class, {ENVIRONMENTS_TABLE}.topology_class),
          placement_scope = COALESCE(EXCLUDED.placement_scope, {ENVIRONMENTS_TABLE}.placement_scope),
          transport_paths = COALESCE(EXCLUDED.transport_paths, {ENVIRONMENTS_TABLE}.transport_paths),
          topology_hops = COALESCE(EXCLUDED.topology_hops, {ENVIRONMENTS_TABLE}.topology_hops),
          latency_sample_count = COALESCE(EXCLUDED.latency_sample_count, {ENVIRONMENTS_TABLE}.latency_sample_count),
          latency_p50_ns = COALESCE(EXCLUDED.latency_p50_ns, {ENVIRONMENTS_TABLE}.latency_p50_ns),
          latency_p99_ns = COALESCE(EXCLUDED.latency_p99_ns, {ENVIRONMENTS_TABLE}.latency_p99_ns),
          latency_p995_ns = COALESCE(EXCLUDED.latency_p995_ns, {ENVIRONMENTS_TABLE}.latency_p995_ns),
          latency_p999_ns = COALESCE(EXCLUDED.latency_p999_ns, {ENVIRONMENTS_TABLE}.latency_p999_ns),
          latency_jitter_p995_ns = COALESCE(EXCLUDED.latency_jitter_p995_ns, {ENVIRONMENTS_TABLE}.latency_jitter_p995_ns),
          frontend = COALESCE(EXCLUDED.frontend, {ENVIRONMENTS_TABLE}.frontend),
          virtualization_family = COALESCE(EXCLUDED.virtualization_family, {ENVIRONMENTS_TABLE}.virtualization_family),
          lane_count = COALESCE(EXCLUDED.lane_count, {ENVIRONMENTS_TABLE}.lane_count),
          worker_count = COALESCE(EXCLUDED.worker_count, {ENVIRONMENTS_TABLE}.worker_count),
          nic_count = COALESCE(EXCLUDED.nic_count, {ENVIRONMENTS_TABLE}.nic_count),
          numa_node_count = COALESCE(EXCLUDED.numa_node_count, {ENVIRONMENTS_TABLE}.numa_node_count),
          lane_mapping = COALESCE(EXCLUDED.lane_mapping, {ENVIRONMENTS_TABLE}.lane_mapping),
          cpu_pinned = COALESCE(EXCLUDED.cpu_pinned, {ENVIRONMENTS_TABLE}.cpu_pinned),
          numa_local = COALESCE(EXCLUDED.numa_local, {ENVIRONMENTS_TABLE}.numa_local),
          benchmark_run_id = CASE WHEN :benchmark_present THEN EXCLUDED.benchmark_run_id ELSE {ENVIRONMENTS_TABLE}.benchmark_run_id END,
          benchmark_status = CASE
            WHEN :benchmark_present
             AND {ENVIRONMENTS_TABLE}.benchmark_run_id = EXCLUDED.benchmark_run_id
             AND {ENVIRONMENTS_TABLE}.benchmark_status = 'complete'
             AND EXCLUDED.benchmark_status = 'collecting'
              THEN {ENVIRONMENTS_TABLE}.benchmark_status
            WHEN :benchmark_present THEN EXCLUDED.benchmark_status
            ELSE {ENVIRONMENTS_TABLE}.benchmark_status
          END,
          benchmark_expected_rails = CASE WHEN :benchmark_present THEN EXCLUDED.benchmark_expected_rails ELSE {ENVIRONMENTS_TABLE}.benchmark_expected_rails END,
          benchmark_completed_rails = CASE
            WHEN :benchmark_present
             AND {ENVIRONMENTS_TABLE}.benchmark_run_id = EXCLUDED.benchmark_run_id
              THEN GREATEST({ENVIRONMENTS_TABLE}.benchmark_completed_rails, EXCLUDED.benchmark_completed_rails)
            WHEN :benchmark_present THEN EXCLUDED.benchmark_completed_rails
            ELSE {ENVIRONMENTS_TABLE}.benchmark_completed_rails
          END,
          benchmark_logical_operations = CASE
            WHEN :benchmark_present
             AND {ENVIRONMENTS_TABLE}.benchmark_run_id = EXCLUDED.benchmark_run_id
             AND {ENVIRONMENTS_TABLE}.benchmark_status = 'complete'
             AND EXCLUDED.benchmark_status = 'collecting'
              THEN {ENVIRONMENTS_TABLE}.benchmark_logical_operations
            WHEN :benchmark_present THEN EXCLUDED.benchmark_logical_operations
            ELSE {ENVIRONMENTS_TABLE}.benchmark_logical_operations
          END,
          benchmark_duration_ns = CASE
            WHEN :benchmark_present
             AND {ENVIRONMENTS_TABLE}.benchmark_run_id = EXCLUDED.benchmark_run_id
             AND {ENVIRONMENTS_TABLE}.benchmark_status = 'complete'
             AND EXCLUDED.benchmark_status = 'collecting'
              THEN {ENVIRONMENTS_TABLE}.benchmark_duration_ns
            WHEN :benchmark_present THEN EXCLUDED.benchmark_duration_ns
            ELSE {ENVIRONMENTS_TABLE}.benchmark_duration_ns
          END,
          benchmark_rails = CASE
            WHEN :benchmark_present
             AND {ENVIRONMENTS_TABLE}.benchmark_run_id = EXCLUDED.benchmark_run_id
             AND {ENVIRONMENTS_TABLE}.benchmark_status = 'complete'
             AND EXCLUDED.benchmark_status = 'collecting'
              THEN {ENVIRONMENTS_TABLE}.benchmark_rails
            WHEN :benchmark_present THEN EXCLUDED.benchmark_rails
            ELSE {ENVIRONMENTS_TABLE}.benchmark_rails
          END
        """,
        parameters=[
            {"name": "public_id", "value": {"stringValue": public_id}},
            {"name": "cloud_provider", "value": {"stringValue": cloud_provider} if cloud_provider else {"isNull": True}},
            {"name": "region", "value": {"stringValue": region} if region else {"isNull": True}},
            {"name": "version", "value": {"stringValue": version} if version else {"isNull": True}},
            {"name": "event_type", "value": {"stringValue": event_type}},
            {"name": "event_increment", "value": {"longValue": event_increment}},
            {"name": "active_volumes", "value": {"longValue": active_volumes} if active_volumes is not None else {"isNull": True}},
            {"name": "recent_iops", "value": {"longValue": recent_iops} if recent_iops is not None else {"isNull": True}},
            {"name": "io_size_bytes", "value": {"longValue": io_size_bytes} if io_size_bytes is not None else {"isNull": True}},
            {"name": "kernel_family", "value": {"stringValue": kernel_family} if kernel_family else {"isNull": True}},
            {"name": "topology_class", "value": {"stringValue": topology_class} if topology_class else {"isNull": True}},
            {"name": "placement_scope", "value": {"stringValue": placement_scope} if placement_scope else {"isNull": True}},
            {"name": "transport_paths", "value": {"stringValue": transport_paths_json} if transport_paths_json else {"isNull": True}},
            {"name": "topology_hops", "value": {"stringValue": topology_hops_json} if topology_hops_json else {"isNull": True}},
            {"name": "latency_sample_count", "value": {"longValue": latency_sample_count} if latency_sample_count is not None else {"isNull": True}},
            {"name": "latency_p50_ns", "value": {"longValue": latency_p50_ns} if latency_p50_ns is not None else {"isNull": True}},
            {"name": "latency_p99_ns", "value": {"longValue": latency_p99_ns} if latency_p99_ns is not None else {"isNull": True}},
            {"name": "latency_p995_ns", "value": {"longValue": latency_p995_ns} if latency_p995_ns is not None else {"isNull": True}},
            {"name": "latency_p999_ns", "value": {"longValue": latency_p999_ns} if latency_p999_ns is not None else {"isNull": True}},
            {"name": "latency_jitter_p995_ns", "value": {"longValue": latency_jitter_p995_ns} if latency_jitter_p995_ns is not None else {"isNull": True}},
            {"name": "frontend", "value": {"stringValue": frontend} if frontend else {"isNull": True}},
            {"name": "virtualization_family", "value": {"stringValue": virtualization_family} if virtualization_family else {"isNull": True}},
            {"name": "lane_count", "value": {"longValue": lane_count} if lane_count is not None else {"isNull": True}},
            {"name": "worker_count", "value": {"longValue": worker_count} if worker_count is not None else {"isNull": True}},
            {"name": "nic_count", "value": {"longValue": nic_count} if nic_count is not None else {"isNull": True}},
            {"name": "numa_node_count", "value": {"longValue": numa_node_count} if numa_node_count is not None else {"isNull": True}},
            {"name": "lane_mapping", "value": {"stringValue": lane_mapping} if lane_mapping else {"isNull": True}},
            {"name": "cpu_pinned", "value": {"booleanValue": cpu_pinned} if cpu_pinned is not None else {"isNull": True}},
            {"name": "numa_local", "value": {"booleanValue": numa_local} if numa_local is not None else {"isNull": True}},
            {"name": "benchmark_run_id", "value": {"stringValue": benchmark_run_id} if benchmark_run_id else {"isNull": True}},
            {"name": "benchmark_status", "value": {"stringValue": benchmark_status} if benchmark_status else {"isNull": True}},
            {"name": "benchmark_expected_rails", "value": {"longValue": benchmark_expected_rails} if benchmark_expected_rails is not None else {"isNull": True}},
            {"name": "benchmark_completed_rails", "value": {"longValue": benchmark_completed_rails} if benchmark_completed_rails is not None else {"isNull": True}},
            {"name": "benchmark_logical_operations", "value": {"longValue": benchmark_logical_operations} if benchmark_logical_operations is not None else {"isNull": True}},
            {"name": "benchmark_duration_ns", "value": {"longValue": benchmark_duration_ns} if benchmark_duration_ns is not None else {"isNull": True}},
            {"name": "benchmark_rails", "value": {"stringValue": benchmark_rails_json} if benchmark_rails_json else {"isNull": True}},
            {"name": "benchmark_present", "value": {"booleanValue": benchmark_state is not None}},
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
          cloud_provider,
          region,
          version,
          last_event_type,
          active_volume_count,
          recent_iops,
          io_size_bytes,
          kernel_family,
          topology_class,
          placement_scope,
          transport_paths,
          topology_hops,
          latency_sample_count,
          latency_p50_ns,
          latency_p99_ns,
          latency_p995_ns,
          latency_p999_ns,
          latency_jitter_p995_ns,
          frontend,
          virtualization_family,
          lane_count,
          worker_count,
          nic_count,
          numa_node_count,
          lane_mapping,
          cpu_pinned,
          numa_local,
          benchmark_run_id,
          benchmark_status,
          benchmark_expected_rails,
          benchmark_completed_rails,
          benchmark_logical_operations,
          benchmark_duration_ns,
          benchmark_rails,
          (COALESCE(lifecycle_active, TRUE)
            AND last_seen >= now() - (:active_hours * interval '1 hour')) AS active
        FROM {ENVIRONMENTS_TABLE}
        ORDER BY last_seen DESC
        LIMIT 500
        """,
        [{"name": "active_hours", "value": {"longValue": ACTIVE_WINDOW_HOURS}}],
    )
    for row in rows:
        run_id = row.pop("benchmark_run_id", None)
        status = row.pop("benchmark_status", None)
        expected_rails = row.pop("benchmark_expected_rails", None)
        completed_rails = row.pop("benchmark_completed_rails", None)
        operations = row.pop("benchmark_logical_operations", None)
        duration_ns = row.pop("benchmark_duration_ns", None)
        rails = row.pop("benchmark_rails", None)
        if run_id:
            row["recent_benchmark"] = {
                "run_id": run_id,
                "status": status,
                "result_scope": "aggregate" if status == "complete" else "rail-set",
                "logical_iops": row.get("recent_iops") if status == "complete" else None,
                "logical_operations": operations,
                "synchronized_duration_ns": duration_ns,
                "expected_rail_count": expected_rails,
                "received_rail_count": completed_rails,
                "aggregation": "sum-logical-operations-over-max-rail-duration",
                "completion_semantics": "remote-application-ack",
                "rails": rails or [],
            }
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
    if event.get("_terraform_bootstrap_schema") is True:
        global _SCHEMA_READY
        _SCHEMA_READY = False
        _ensure_schema()
        return {"schema_ready": True}

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
        elif validation_error := _validate_non_identifying_telemetry(event_item):
            invalid_events.append({"index": index, "error": validation_error})

    if invalid_events:
        return _response(400, {"error": "invalid_event_stream", "invalid_events": invalid_events})

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

            parameter_sets.append(_event_parameters(event_id, event_type, region, payload))
            anonymous_installation_id = payload_event.get("anonymous_installation_id")
            if isinstance(anonymous_installation_id, str) and anonymous_installation_id.strip():
                environment_key = anonymous_installation_id.strip()
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
        benchmark_updates = {}
        for payload_event in body_json:
            anonymous_installation_id = payload_event.get("anonymous_installation_id")
            if not isinstance(anonymous_installation_id, str) or not anonymous_installation_id.strip():
                continue
            benchmark_state = _record_benchmark_rail(payload_event)
            if benchmark_state is not None:
                benchmark_updates[anonymous_installation_id.strip()] = benchmark_state
        for payload_event, event_type, region, event_increment in environment_updates.values():
            try:
                anonymous_installation_id = payload_event.get("anonymous_installation_id", "").strip()
                _upsert_environment(
                    payload_event,
                    event_type,
                    region,
                    event_increment,
                    benchmark_updates.get(anonymous_installation_id),
                )
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
