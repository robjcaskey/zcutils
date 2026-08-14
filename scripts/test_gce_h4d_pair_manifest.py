from __future__ import annotations

import argparse
import importlib.util
import pathlib
import sys
import unittest
from unittest import mock


SCRIPT = pathlib.Path(__file__).with_name("gce-h4d-pair-manifest.py")
SPEC = importlib.util.spec_from_file_location("gce_h4d_pair_manifest", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
manifest_module = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = manifest_module
SPEC.loader.exec_module(manifest_module)


class PairManifestTests(unittest.TestCase):
    project = "test-project"
    zone = "us-central1-a"
    region = "us-central1"
    policy = "test-compact"
    network = "test-falcon"
    subnet = "test-falcon-sub"

    def instance(self, name: str, rdma_ip: str, control_ip: str) -> dict[str, object]:
        return {
            "name": name,
            "status": "RUNNING",
            "zone": f"zones/{self.zone}",
            "machineType": "machineTypes/h4d-standard-192",
            "resourcePolicies": [f"resourcePolicies/{self.policy}"],
            "scheduling": {
                "provisioningModel": "SPOT",
                "onHostMaintenance": "TERMINATE",
                "instanceTerminationAction": "DELETE",
                "maxRunDuration": {"seconds": "3000"},
            },
            "networkInterfaces": [
                {
                    "nicType": "GVNIC",
                    "networkIP": control_ip,
                    "accessConfigs": [{"natIP": "203.0.113.10"}],
                },
                {
                    "nicType": "IRDMA",
                    "networkIP": rdma_ip,
                    "network": f"networks/{self.network}",
                    "subnetwork": f"subnetworks/{self.subnet}",
                    "stackType": "IPV4_ONLY",
                },
            ],
            "metadata": {
                "items": [
                    {"key": "bulk_traffic_policy", "value": "irdma-same-zone-only"},
                    {
                        "key": "adhoc_hourly_instance_price_usd",
                        "value": "3.29832",
                    },
                    {
                        "key": "adhoc_estimated_total_cost_usd",
                        "value": "5.497200",
                    },
                    {"key": "adhoc_max_total_cost_usd", "value": "6"},
                ]
            },
        }

    def args(self) -> argparse.Namespace:
        return argparse.Namespace(
            project=self.project,
            zone=self.zone,
            instance=["node-00", "node-01"],
            placement_policy=self.policy,
        )

    def test_valid_pair_proves_same_zone_falcon_and_cost_policy(self) -> None:
        instances = [
            self.instance("node-00", "10.40.0.2", "10.128.0.2"),
            self.instance("node-01", "10.40.0.3", "10.128.0.3"),
        ]
        resources = [
            {
                "mtu": 8896,
                "networkProfile": f"networkProfiles/{self.zone}-vpc-falcon",
                "routingConfig": {"routingMode": "REGIONAL"},
            },
            {
                "region": f"regions/{self.region}",
                "network": f"networks/{self.network}",
                "ipCidrRange": "10.40.0.0/24",
                "privateIpGoogleAccess": False,
            },
            {
                "status": "READY",
                "groupPlacementPolicy": {
                    "collocation": "COLLOCATED",
                    "maxDistance": 3,
                    "vmCount": 2,
                },
            },
        ]
        with mock.patch.object(
            manifest_module, "describe_instance", side_effect=instances
        ), mock.patch.object(manifest_module, "run_json", side_effect=resources):
            manifest = manifest_module.build_manifest(self.args())
        self.assertEqual(manifest["zone"], self.zone)
        self.assertEqual(manifest["rdma_network"]["profile"], f"{self.zone}-vpc-falcon")
        self.assertFalse(manifest["bulk_traffic_policy"]["cross_region"])
        self.assertFalse(manifest["bulk_traffic_policy"]["internet"])
        self.assertEqual(
            manifest["bulk_traffic_policy"]["allowed_peer_ipv4"],
            ["10.40.0.2", "10.40.0.3"],
        )
        self.assertEqual(manifest["cost"]["estimated_pair_cost_usd"], "5.497200")

    def test_external_address_on_irdma_is_rejected(self) -> None:
        instance = self.instance("node-00", "10.40.0.2", "10.128.0.2")
        instance["networkInterfaces"][1]["accessConfigs"] = [
            {"natIP": "203.0.113.20"}
        ]
        with self.assertRaisesRegex(SystemExit, "exposes the IRDMA interface externally"):
            manifest_module.validate_instance(
                instance, self.project, self.zone, self.policy
            )


if __name__ == "__main__":
    unittest.main()
