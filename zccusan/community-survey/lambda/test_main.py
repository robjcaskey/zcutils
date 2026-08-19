import importlib.util
import json
import os
import sys
import types
import unittest
from pathlib import Path


class FakeRdsData:
    def __init__(self):
        self.statements = []
        self.batches = []

    def execute_statement(self, **kwargs):
        self.statements.append(kwargs)
        if kwargs.get("formatRecordsAs") == "JSON":
            return {
                "formattedRecords": json.dumps([
                    {
                        "public_environment_id": "env-0123456789ab",
                        "first_seen_ms": 1,
                        "last_seen_ms": 2,
                        "event_count": 3,
                        "active": True,
                    }
                ])
            }
        return {}

    def batch_execute_statement(self, **kwargs):
        self.batches.append(kwargs)
        return {}


def load_lambda(fake_rds):
    fake_boto3 = types.SimpleNamespace(client=lambda _service: fake_rds)
    sys.modules["boto3"] = fake_boto3
    os.environ.update({
        "SURVEY_DB_CLUSTER_ARN": "cluster",
        "SURVEY_DB_SECRET_ARN": "secret",
        "SURVEY_DB_NAME": "community_survey",
    })
    module_path = Path(__file__).with_name("main.py")
    spec = importlib.util.spec_from_file_location("survey_lambda_under_test", module_path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class SurveyLambdaTests(unittest.TestCase):
    def setUp(self):
        self.rds = FakeRdsData()
        self.app = load_lambda(self.rds)

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
        self.assertEqual(len(self.rds.batches), 1)
        raw_parameters = self.rds.batches[0]["parameterSets"][0]
        raw_payload = next(p for p in raw_parameters if p["name"] == "payload")
        self.assertIn("private-environment-id", raw_payload["value"]["stringValue"])
        projection = self.rds.statements[-1]
        projection_text = json.dumps(projection)
        self.assertNotIn("private-environment-id", projection_text)
        self.assertIn("env-", projection_text)

    def test_rejects_individual_events_larger_than_four_kibibytes(self):
        response = self.app.handler({
            "body": json.dumps({"payload": "x" * 4097}),
            "requestContext": {"http": {"method": "POST", "path": "/survey"}},
        }, None)
        self.assertEqual(response["statusCode"], 400)
        self.assertEqual(len(self.rds.batches), 0)


if __name__ == "__main__":
    unittest.main()
