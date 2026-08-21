import importlib.util
import json
import os
import unittest
from pathlib import Path


class FakeDsqlStore:
    def __init__(self):
        self.statements = []
        self.batches = []

    def execute(self, sql, parameters=None):
        self.statements.append({"sql": sql, "parameters": parameters or []})

    def query(self, _sql, _parameters=None):
        return [{
            "public_environment_id": "env-0123456789ab",
            "first_seen_ms": 1,
            "last_seen_ms": 2,
            "event_count": 3,
            "cloud_provider": "aws",
            "region": "us-east-1",
            "frontend": "linux-block",
            "io_size_bytes": 4096,
            "kernel_family": "linux-6.17.0-1017-aws",
            "virtualization_family": "nitro-vm",
            "topology_class": "client-leaf",
            "placement_scope": "same-placement-group",
            "transport_paths": {"efa-direct": 2},
            "topology_hops": [{"ordinal": 1, "role": "storage-service", "transport": "efa-direct"}],
            "lane_count": 64,
            "worker_count": 64,
            "nic_count": 2,
            "numa_node_count": 2,
            "lane_mapping": "one-lane-per-worker",
            "cpu_pinned": True,
            "numa_local": True,
            "latency_sample_count": 10000,
            "latency_p50_ns": 1000,
            "latency_p99_ns": 2000,
            "latency_p995_ns": 2500,
            "latency_p999_ns": 3000,
            "latency_jitter_p995_ns": 1500,
            "active": True,
        }]

    def insert(self, parameter_sets):
        self.batches.append(parameter_sets)


def load_lambda(fake_store):
    os.environ.update({
        "SURVEY_DSQL_ENDPOINT": "test.dsql.us-east-1.on.aws",
        "SURVEY_DB_NAME": "postgres",
    })
    module_path = Path(__file__).with_name("main.py")
    spec = importlib.util.spec_from_file_location("survey_lambda_under_test", module_path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module._execute_sql = fake_store.execute
    module._query_sql = fake_store.query
    module._insert_events = fake_store.insert
    module._SCHEMA_READY = True
    return module


class SurveyLambdaTests(unittest.TestCase):
    def setUp(self):
        self.store = FakeDsqlStore()
        self.app = load_lambda(self.store)

    def test_dashboard_contains_no_raw_event_data(self):
        response = self.app.handler({
            "requestContext": {"http": {"method": "GET", "path": "/dashboard"}}
        }, None)
        self.assertEqual(response["statusCode"], 200)
        self.assertIn("community pulse", response["body"])
        self.assertIn("e.cloud_provider", response["body"])
        self.assertIn("e.region", response["body"])
        self.assertNotIn("source_ip_hash", response["body"])

    def test_public_environment_api_only_returns_projection(self):
        response = self.app.handler({
            "requestContext": {"http": {"method": "GET", "path": "/community/environments"}}
        }, None)
        body = json.loads(response["body"])
        self.assertEqual(response["statusCode"], 200)
        self.assertEqual(body["environments"][0]["public_environment_id"], "env-0123456789ab")
        self.assertEqual(body["environments"][0]["cloud_provider"], "aws")
        self.assertEqual(body["environments"][0]["region"], "us-east-1")
        self.assertEqual(body["environments"][0]["frontend"], "linux-block")
        self.assertEqual(body["environments"][0]["io_size_bytes"], 4096)
        self.assertEqual(body["environments"][0]["transport_paths"], {"efa-direct": 2})
        self.assertNotIn("payload", body["environments"][0])

    def test_ingest_persists_only_validated_non_identifying_telemetry(self):
        event = {
            "telemetry_schema_version": 1,
            "anonymization_schema_version": 1,
            "event_type": "csi_hourly_stats",
            "anonymous_installation_id": "anon-" + "a" * 64,
            "cloud_provider": "aws",
            "cloud_region": "us-east-1",
            "active_volume_count": 4,
            "total_iops": 12345,
            "io_size_bytes": 4096,
        }
        response = self.app.handler({
            "body": json.dumps(event),
            "requestContext": {
                "http": {"method": "POST", "path": "/survey", "sourceIp": "192.0.2.1"}
            },
        }, None)
        self.assertEqual(response["statusCode"], 200)
        self.assertEqual(len(self.store.batches), 1)
        raw_parameters = self.store.batches[0][0]
        raw_payload = next(p for p in raw_parameters if p["name"] == "payload")
        self.assertIn("anon-" + "a" * 64, raw_payload["value"]["stringValue"])
        self.assertNotIn("source_ip_hash", {p["name"] for p in raw_parameters})
        projection = self.store.statements[-1]
        projection_text = json.dumps(projection)
        self.assertIn("env-", projection_text)
        self.assertIn("aws", projection_text)

    def test_identifying_or_unknown_telemetry_is_rejected_before_storage(self):
        event = {
            "telemetry_schema_version": 0,
            "anonymization_schema_version": 1,
            "event_type": "legacy_signal",
            "environment_id": "private-environment-id",
            "node_id": "private-node",
        }
        response = self.app.handler({
            "body": json.dumps(event),
            "requestContext": {"http": {"method": "POST", "path": "/survey"}},
        }, None)
        body = json.loads(response["body"])
        self.assertEqual(response["statusCode"], 400)
        self.assertIn("fields_not_permitted", body["invalid_events"][0]["error"])
        self.assertEqual(self.store.batches, [])

    def test_future_telemetry_fields_require_anonymizer_schema_upgrade(self):
        event = {
            "telemetry_schema_version": 99,
            "anonymization_schema_version": 1,
            "event_type": "future_signal",
            "future_safe_someday": 7,
        }
        response = self.app.handler({
            "body": json.dumps(event),
            "requestContext": {"http": {"method": "POST", "path": "/survey"}},
        }, None)
        self.assertEqual(response["statusCode"], 400)
        self.assertEqual(self.store.batches, [])

    def test_rejects_individual_events_larger_than_four_kibibytes(self):
        response = self.app.handler({
            "body": json.dumps({"payload": "x" * 4097}),
            "requestContext": {"http": {"method": "POST", "path": "/survey"}},
        }, None)
        self.assertEqual(response["statusCode"], 400)
        self.assertEqual(len(self.store.batches), 0)

    def test_named_parameters_preserve_postgres_casts(self):
        sql = self.app._psycopg_sql("VALUES (:payload::jsonb, :active_hours)")
        self.assertEqual(sql, "VALUES (%(payload)s::jsonb, %(active_hours)s)")

    def test_terraform_bootstrap_initializes_schema_outside_request_path(self):
        self.app._SCHEMA_READY = False
        response = self.app.handler({"_terraform_bootstrap_schema": True}, None)
        self.assertEqual(response, {"schema_ready": True})
        self.assertTrue(self.app._SCHEMA_READY)
        self.assertGreaterEqual(len(self.store.statements), 5)
        self.assertTrue(any("frontend" in statement["sql"] for statement in self.store.statements))

    def test_zero_active_volumes_immediately_closes_environment(self):
        event = {
            "telemetry_schema_version": 1,
            "anonymization_schema_version": 1,
            "event_type": "volume_disconnected",
            "anonymous_installation_id": "anon-" + "b" * 64,
            "active_volume_count": 0,
        }
        response = self.app.handler({
            "body": json.dumps(event),
            "requestContext": {"http": {"method": "POST", "path": "/survey"}},
        }, None)
        self.assertEqual(response["statusCode"], 200)
        projection = self.store.statements[-1]
        self.assertIn("lifecycle_active", projection["sql"])
        active_volumes = next(
            parameter for parameter in projection["parameters"]
            if parameter["name"] == "active_volumes"
        )
        self.assertEqual(active_volumes["value"]["longValue"], 0)


if __name__ == "__main__":
    unittest.main()
