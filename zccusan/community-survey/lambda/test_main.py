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
        self.assertNotIn("source_ip_hash", response["body"])

    def test_public_environment_api_only_returns_projection(self):
        response = self.app.handler({
            "requestContext": {"http": {"method": "GET", "path": "/community/environments"}}
        }, None)
        body = json.loads(response["body"])
        self.assertEqual(response["statusCode"], 200)
        self.assertEqual(body["environments"][0]["public_environment_id"], "env-0123456789ab")
        self.assertNotIn("payload", body["environments"][0])

    def test_ingest_keeps_raw_json_and_updates_sanitized_projection(self):
        event = {
            "event_type": "csi_hourly_stats",
            "environment_id": "private-environment-id",
            "active_volume_count": 4,
            "total_iops": 12345,
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
        self.assertIn("private-environment-id", raw_payload["value"]["stringValue"])
        projection = self.store.statements[-1]
        projection_text = json.dumps(projection)
        self.assertNotIn("private-environment-id", projection_text)
        self.assertIn("env-", projection_text)

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
        self.assertEqual(len(self.store.statements), 2)


if __name__ == "__main__":
    unittest.main()
