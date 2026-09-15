import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location('runner_monitor', Path(__file__).with_name('test-github-ec2-runner.py'))
monitor = importlib.util.module_from_spec(spec)
spec.loader.exec_module(monitor)


class MonitorTests(unittest.TestCase):
    def test_transient_get_failure_is_retried(self):
        bad = subprocess.CompletedProcess([], 1, '', 'net/http: TLS handshake timeout')
        good = subprocess.CompletedProcess([], 0, '{"status":"in_progress"}', '')
        with mock.patch.object(monitor.subprocess, 'run', side_effect=[bad, good]) as run, \
                mock.patch.object(monitor.time, 'sleep'):
            self.assertEqual(monitor.api('repos/test/actions/runs/1')['status'], 'in_progress')
            self.assertEqual(run.call_count, 2)

    def test_mutation_is_never_retried(self):
        bad = subprocess.CompletedProcess([], 1, '', 'HTTP 503')
        with mock.patch.object(monitor.subprocess, 'run', return_value=bad) as run:
            with self.assertRaises(RuntimeError):
                monitor.api('repos/test/actions/dispatches', 'POST', {})
            self.assertEqual(run.call_count, 1)

    def test_read_timeout_is_retried_but_auth_failure_is_not(self):
        good = subprocess.CompletedProcess([], 0, '{}', '')
        with mock.patch.object(monitor.subprocess, 'run', side_effect=[subprocess.TimeoutExpired('gh', 60), good]), \
                mock.patch.object(monitor.time, 'sleep'):
            self.assertEqual(monitor.api('repos/test'), {})
        bad = subprocess.CompletedProcess([], 1, '', 'HTTP 403')
        with mock.patch.object(monitor.subprocess, 'run', return_value=bad) as run:
            with self.assertRaises(RuntimeError):
                monitor.api('repos/test')
            self.assertEqual(run.call_count, 1)

    def test_repeated_read_failure_has_a_bound(self):
        bad = subprocess.CompletedProcess([], 1, '', 'HTTP 503')
        with mock.patch.object(monitor.subprocess, 'run', return_value=bad) as run, \
                mock.patch.object(monitor.time, 'sleep'):
            with self.assertRaises(RuntimeError):
                monitor.api('repos/test')
            self.assertEqual(run.call_count, 5)
