import importlib.util
import json
import os
import unittest
from pathlib import Path


class FakeDsqlStore:
    def __init__(self):
        self.statements = []
        self.batches = []
        self.benchmark_rails = {}

    def execute(self, sql, parameters=None):
        self.statements.append({"sql": sql, "parameters": parameters or []})
        if "INSERT INTO community_benchmark_rails" in sql:
            values = self._values(parameters)
            key = (values["public_id"], values["run_id"], values["rail_index"])
            self.benchmark_rails[key] = {
                "rail_index": values["rail_index"],
                "expected_rail_count": values["rail_count"],
                "logical_operations": values["operations"],
                "measurement_duration_ns": values["duration_ns"],
                "reported_iops": values["reported_iops"],
                **{
                    name: values.get(name)
                    for name in (
                        "size_bytes", "latency_sample_count", "latency_p50_ns",
                        "latency_p99_ns", "latency_p995_ns", "latency_p999_ns",
                        "latency_jitter_p995_ns",
                    )
                },
            }

    @staticmethod
    def _values(parameters):
        values = {}
        for parameter in parameters or []:
            encoded = parameter["value"]
            values[parameter["name"]] = (
                None if encoded.get("isNull") else next(iter(encoded.values()))
            )
        return values

    def query(self, sql, parameters=None):
        if "FROM community_benchmark_rails" in sql:
            values = self._values(parameters)
            rows = [
                row for (environment_id, run_id, _), row in self.benchmark_rails.items()
                if environment_id == values["public_id"] and run_id == values["run_id"]
            ]
            return sorted(rows, key=lambda row: row["rail_index"])
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
            "recent_iops": 17_935_873,
            "benchmark_run_id": "run-0123456789abcdef",
            "benchmark_status": "complete",
            "benchmark_expected_rails": 2,
            "benchmark_completed_rails": 2,
            "benchmark_logical_operations": 83_886_080,
            "benchmark_duration_ns": 4_677_000_000,
            "benchmark_rails": [
                {"rail_index": 0, "logical_iops": 8_969_696},
                {"rail_index": 1, "logical_iops": 8_967_936},
            ],
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
        benchmark = body["environments"][0]["recent_benchmark"]
        self.assertEqual(benchmark["logical_iops"], 17_935_873)
        self.assertEqual(benchmark["expected_rail_count"], 2)
        self.assertEqual(len(benchmark["rails"]), 2)
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

    def test_sender_rails_form_one_exact_logical_benchmark(self):
        common = {
            "telemetry_schema_version": 2,
            "anonymization_schema_version": 2,
            "event_type": "transport_benchmark_result",
            "anonymous_installation_id": "anon-" + "c" * 64,
            "anonymous_benchmark_run_id": "anonrun-" + "d" * 64,
            "benchmark_result_scope": "rail",
            "benchmark_rail_count": 2,
            "logical_operations": 41_943_040,
            "active_volume_count": 0,
            "io_size_bytes": 4096,
            "latency_scope": "remote-application-ack",
        }
        rail0 = {
            **common,
            "benchmark_rail_index": 0,
            "measurement_duration_ns": 4_676_083_000,
        }
        rail1 = {
            **common,
            "benchmark_rail_index": 1,
            "measurement_duration_ns": 4_677_000_000,
        }
        for rail in (rail0, rail1):
            rail["total_iops"] = (
                rail["logical_operations"] * 1_000_000_000
                + rail["measurement_duration_ns"] // 2
            ) // rail["measurement_duration_ns"]

        response = self.app.handler({
            "body": json.dumps([rail0, rail1]),
            "requestContext": {"http": {"method": "POST", "path": "/survey"}},
        }, None)

        self.assertEqual(response["statusCode"], 200)
        self.assertEqual(len(self.store.benchmark_rails), 2)
        projection = self.store.statements[-1]
        values = self.store._values(projection["parameters"])
        self.assertEqual(values["recent_iops"], 17_935_873)
        self.assertEqual(values["benchmark_expected_rails"], 2)
        self.assertEqual(values["benchmark_completed_rails"], 2)
        self.assertEqual(values["benchmark_logical_operations"], 83_886_080)
        self.assertEqual(values["benchmark_duration_ns"], 4_677_000_000)
        rails = json.loads(values["benchmark_rails"])
        self.assertEqual([rail["rail_index"] for rail in rails], [0, 1])

    def test_sender_rail_with_rounded_iops_not_matching_counts_is_rejected(self):
        event = {
            "telemetry_schema_version": 2,
            "anonymization_schema_version": 2,
            "event_type": "transport_benchmark_result",
            "anonymous_installation_id": "anon-" + "e" * 64,
            "anonymous_benchmark_run_id": "anonrun-" + "f" * 64,
            "benchmark_result_scope": "rail",
            "benchmark_rail_index": 0,
            "benchmark_rail_count": 2,
            "logical_operations": 100,
            "measurement_duration_ns": 1_000_000_000,
            "total_iops": 999,
        }
        response = self.app.handler({
            "body": json.dumps(event),
            "requestContext": {"http": {"method": "POST", "path": "/survey"}},
        }, None)
        body = json.loads(response["body"])
        self.assertEqual(response["statusCode"], 400)
        self.assertEqual(
            body["invalid_events"][0]["error"], "inconsistent_benchmark_iops"
        )


if __name__ == "__main__":
    unittest.main()
