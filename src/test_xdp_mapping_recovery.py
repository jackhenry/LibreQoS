import unittest

import LibreQoS


class FakeIpMapBatch:
    def __init__(self, submit_error=None):
        self.finished = False
        self.submitted = False
        self.submit_error = submit_error

    def finish_ip_mappings(self):
        self.finished = True

    def submit(self):
        self.submitted = True
        if self.submit_error is not None:
            raise self.submit_error


class XdpMappingRecoveryTests(unittest.TestCase):
    def test_ready_on_first_check_does_not_sleep(self):
        sleeps = []

        LibreQoS.wait_for_xdp_ip_mapping_ready(
            ready=lambda: True,
            sleep=lambda seconds: sleeps.append(seconds),
            now=lambda: 0.0,
        )

        self.assertEqual(sleeps, [])

    def test_readiness_after_one_false_check_sleeps_once(self):
        checks = iter([False, True])
        sleeps = []
        times = iter([0.0, 0.0, 0.0])

        LibreQoS.wait_for_xdp_ip_mapping_ready(
            ready=lambda: next(checks),
            sleep=lambda seconds: sleeps.append(seconds),
            now=lambda: next(times),
            timeout_seconds=5.0,
            interval_seconds=0.1,
        )

        self.assertEqual(sleeps, [0.1])

    def test_persistent_false_readiness_times_out(self):
        times = iter([0.0, 0.0, 0.2])

        with self.assertRaises(TimeoutError):
            LibreQoS.wait_for_xdp_ip_mapping_ready(
                ready=lambda: False,
                sleep=lambda _seconds: None,
                now=lambda: next(times),
                timeout_seconds=0.1,
                interval_seconds=0.1,
            )

    def test_readiness_os_error_reports_before_submit(self):
        batch = FakeIpMapBatch()
        reports = []

        def raise_os_error():
            raise OSError("Unable to inspect XDP IP mapping maps")

        LibreQoS.apply_xdp_ip_mappings(
            batch,
            {"queued_requests": 1},
            ready=raise_os_error,
            sleep=lambda _seconds: None,
            report_failure=lambda *args: reports.append(args),
            clear_recovered_issue=lambda *_args: True,
        )

        self.assertFalse(batch.finished)
        self.assertFalse(batch.submitted)
        self.assertEqual(reports[0][0], LibreQoS.XDP_IP_MAPPING_APPLY_FAILED)

    def test_submit_failure_reports_mapping_apply_failure(self):
        batch = FakeIpMapBatch(submit_error=OSError("Unable to open BPF map"))
        reports = []

        LibreQoS.apply_xdp_ip_mappings(
            batch,
            {"queued_requests": 1},
            ready=lambda: True,
            sleep=lambda _seconds: None,
            report_failure=lambda *args: reports.append(args),
            clear_recovered_issue=lambda *_args: True,
        )

        self.assertTrue(batch.finished)
        self.assertTrue(batch.submitted)
        self.assertEqual(reports[0][0], LibreQoS.XDP_IP_MAPPING_APPLY_FAILED)

    def test_success_clears_recovered_mapping_issue(self):
        batch = FakeIpMapBatch()
        cleared = []

        LibreQoS.apply_xdp_ip_mappings(
            batch,
            {"queued_requests": 1},
            ready=lambda: True,
            sleep=lambda _seconds: None,
            report_failure=lambda *_args: self.fail("unexpected failure report"),
            clear_recovered_issue=lambda code, dedupe_key: cleared.append((code, dedupe_key)) or True,
        )

        self.assertTrue(batch.finished)
        self.assertTrue(batch.submitted)
        self.assertEqual(
            cleared,
            [
                (
                    LibreQoS.XDP_IP_MAPPING_APPLY_FAILED,
                    LibreQoS.XDP_IP_MAPPING_APPLY_FAILED,
                )
            ],
        )


if __name__ == "__main__":
    unittest.main()
