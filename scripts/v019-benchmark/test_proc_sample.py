#!/usr/bin/env python3
"""Deterministic tests for proc_sample parsing and the shared code-page budget."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

import proc_sample


class ProcSampleTests(unittest.TestCase):
    def test_smaps_aggregates_path_permissions_and_guard_pages(self) -> None:
        text = """00400000-00401000 r-xp 00000000 00:00 1 /tmp/rpi
Rss: 4 kB
Pss: 2 kB
Shared_Clean: 4 kB
Private_Dirty: 0 kB
00401000-00402000 r-xp 00000000 00:00 1 /tmp/rpi
Rss: 8 kB
Pss: 4 kB
Shared_Dirty: 8 kB
00402000-00403000 ---p 00000000 00:00 0
Rss: 0 kB
Pss: 0 kB
"""
        rows = proc_sample.aggregate_mappings(proc_sample.parse_smaps(text))
        self.assertEqual(len(rows), 2)
        guard = next(row for row in rows if row["permissions"] == "---p")
        code = next(row for row in rows if row["path"] == "/tmp/rpi")
        self.assertEqual(guard["path"], "[anonymous]")
        self.assertEqual(code["rssKib"], 12)
        self.assertEqual(code["sharedKib"], 12)
        self.assertEqual(code["segments"], 2)

    def test_code_budget_is_shared_and_fail_closed(self) -> None:
        report = {
            "deltas": [
                {
                    "path": "/tmp/rpi",
                    "permissions": "r-xp",
                    "sharedKibDelta": 1024,
                    "rssKibDelta": 1024,
                    "pssKibDelta": 0,
                    "existing": True,
                    "readOnlyCode": True,
                },
                {
                    "path": "/tmp/librpi_ext_statusline.so",
                    "permissions": "r-xp",
                    "sharedKibDelta": 1024,
                    "rssKibDelta": 1024,
                    "pssKibDelta": 0,
                    "existing": False,
                    "readOnlyCode": True,
                },
            ]
        }
        result = proc_sample.check_budget(report)
        self.assertEqual(result["newReadOnlyCodeSharedKib"], 2048)
        self.assertEqual(result["newReadOnlyCodeRssFileKib"], 2048)
        report["deltas"][1]["sharedKibDelta"] = 1025
        with self.assertRaises(ValueError):
            proc_sample.check_budget(report)

    def test_top_virt_res_shr_are_parsed_without_adding_shr_to_res(self) -> None:
        text = """    PID USER PR NI VIRT RES SHR S %CPU %MEM TIME+ COMMAND
123 leven 20 0 1.5g 256m 23032 S 1.0 1.0 0:01.00 rpi
"""
        metrics = proc_sample.parse_top(text, 123)
        self.assertEqual(metrics["virtKib"], 1.5 * 1024 * 1024)
        self.assertEqual(metrics["resKib"], 256 * 1024)
        self.assertEqual(metrics["shrKib"], 23032)

    def test_same_path_replacement_binary_is_new_code_not_existing_growth(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            baseline = root / "baseline.json"
            candidate = root / "candidate.json"
            baseline.write_text(json.dumps({"mappings": [{
                "path": "/tmp/rpi", "permissions": "r-xp", "device": "08:01", "inode": 10,
                "sharedKib": 100, "rssKib": 100, "pssKib": 100,
            }]}))
            candidate.write_text(json.dumps({"mappings": [{
                "path": "/tmp/rpi", "permissions": "r-xp", "device": "08:01", "inode": 20,
                "sharedKib": 140, "rssKib": 140, "pssKib": 140,
            }]}))
            report = proc_sample.compare(baseline, candidate)
            added = next(delta for delta in report["deltas"] if delta["sharedKibDelta"] > 0)
            self.assertEqual(added["candidateIdentities"], [("08:01", 20)])
            self.assertTrue(added["replacement"])
            self.assertFalse(added["existing"])
            self.assertEqual(report["budgetCheck"]["existingSharedKibGrowth"], 0)
            self.assertEqual(report["budgetCheck"]["newReadOnlyCodeSharedKib"], 40)

    def test_legacy_summaries_keep_path_based_comparison(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            baseline = root / "baseline.json"
            candidate = root / "candidate.json"
            payload = {"mappings": [{
                "path": "/tmp/rpi", "permissions": "r-xp",
                "sharedKib": 100, "rssKib": 100, "pssKib": 100,
            }]}
            baseline.write_text(json.dumps(payload))
            candidate.write_text(json.dumps(payload))
            report = proc_sample.compare(baseline, candidate)
            self.assertEqual(len(report["deltas"]), 1)
            self.assertTrue(report["deltas"][0]["existing"])

    def test_new_non_code_mapping_is_reported_not_exempted(self) -> None:
        report = {
            "deltas": [
                {
                    "path": "[anonymous]",
                    "permissions": "rw-p",
                    "sharedKibDelta": 64,
                    "rssKibDelta": 64,
                    "pssKibDelta": 64,
                    "existing": False,
                    "readOnlyCode": False,
                }
            ]
        }
        self.assertEqual(proc_sample.check_budget(report)["newNonCodeSharedKib"], 64)

    def test_attribution_covers_positive_growth_and_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            baseline = root / "baseline.json"
            candidate = root / "candidate.json"
            baseline.write_text(json.dumps({"mappings": [
                {"path": "/usr/lib/libm.so.6", "permissions": "r--p", "device": "08:01", "inode": 10, "sharedKib": 0, "rssKib": 0, "pssKib": 0},
                {"path": "/tmp/rpi", "permissions": "r--p", "device": "08:01", "inode": 20, "sharedKib": 0, "rssKib": 0, "pssKib": 0},
            ]}))
            candidate.write_text(json.dumps({"mappings": [
                {"path": "/usr/lib/libm.so.6", "permissions": "r--p", "device": "08:01", "inode": 10, "sharedKib": 8, "rssKib": 8, "pssKib": 0},
                {"path": "/tmp/rpi", "permissions": "r--p", "device": "08:01", "inode": 30, "sharedKib": 4, "rssKib": 4, "pssKib": 4},
            ]}))
            attribution = {
                "jsonschemaCodegen": {"source": "generated meta-validator"},
                "otherV019ReadOnlyCode": {"source": "remaining V19-01 code"},
                "existingMappings": [{
                    "path": "/usr/lib/libm.so.6",
                    "permissions": "r--p",
                    "reason": "unchanged system library; resident-page sampling variation",
                }],
                "newNonCodeMappings": [{
                    "path": "/tmp/rpi",
                    "permissions": "r--p",
                    "reason": "same-path replacement binary metadata pages",
                }],
            }
            report = proc_sample.compare(baseline, candidate, attribution)
            self.assertTrue(report["attributionComplete"])
            self.assertEqual(report["budgetCheck"]["existingSharedKibGrowth"], 8)
            self.assertEqual(report["budgetCheck"]["newNonCodeSharedKib"], 4)
            attribution["newNonCodeMappings"] = []
            with self.assertRaisesRegex(ValueError, "/tmp/rpi"):
                proc_sample.compare(baseline, candidate, attribution)


if __name__ == "__main__":
    unittest.main()
