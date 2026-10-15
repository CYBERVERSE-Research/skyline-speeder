import json
import sys
import tempfile
import unittest
from pathlib import Path

EXPERIMENT_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(EXPERIMENT_DIR))

from render_kernel_compat_report import (
    build_summary,
    load_campaign_versions,
    render_artifacts_table,
    render_capabilities_table,
    render_main_table,
    render_report,
)


def _write_state(root, version, **overrides):
    state = {
        "version": version,
        "overall": "pass",
        "identity": {"repo_rev": "abc123", "repo_dirty": False},
        "stages": {
            "fetch": {"status": "pass", "detail": ""},
            "build": {"status": "pass", "detail": ""},
            "boot": {"status": "pass", "detail": ""},
            "uname-gate": {"status": "pass", "detail": version},
            "bpf-compile": {"status": "pass", "detail": ""},
            "skyline-deploy": {"status": "pass", "detail": ""},
            "smoke-static": {"status": "pass", "detail": ""},
            "smoke-v4": {"status": "pass", "detail": ""},
            "smoke-v6": {"status": "skip", "detail": "ipv6 matrix support not landed"},
            "teardown": {"status": "pass", "detail": ""},
        },
        "capabilities": {
            "btf": True,
            "bpffs": True,
            "cgroup_v2": True,
            "fq_available": True,
            "struct_ops": True,
            "rack_reo_hook": False,
            "fallback_cc_available": True,
            "notes": [],
        },
        "artifacts": {
            "config_sha256": "c" * 64,
            "vmlinux_sha256": "v" * 64,
            "vmlinux_h_sha256": f"h{version}".ljust(64, "0"),
            "build_seconds": 3000,
        },
    }
    state.update(overrides)
    version_dir = root / "versions" / version
    version_dir.mkdir(parents=True)
    (version_dir / "state.json").write_text(json.dumps(state), encoding="utf-8")
    return state


class KernelCompatReportTests(unittest.TestCase):
    def test_load_campaign_versions_reads_all_state_files_sorted(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _write_state(root, "7.1.6")
            _write_state(root, "6.1.180")
            versions = load_campaign_versions(root)
        self.assertEqual([v["version"] for v in versions], ["6.1.180", "7.1.6"])

    def test_load_campaign_versions_missing_dir_returns_empty(self):
        with tempfile.TemporaryDirectory() as directory:
            versions = load_campaign_versions(Path(directory))
        self.assertEqual(versions, [])

    def test_load_campaign_versions_survives_corrupt_state_json(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            version_dir = root / "versions" / "6.6.148"
            version_dir.mkdir(parents=True)
            (version_dir / "state.json").write_text("{not valid json", encoding="utf-8")
            versions = load_campaign_versions(root)
        self.assertEqual(len(versions), 1)
        self.assertEqual(versions[0]["overall"], "fail")
        self.assertIn("not valid JSON", versions[0]["failure"]["message"])

    def test_build_summary_reads_campaign_json_when_present(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "campaign.json").write_text(
                json.dumps({"git_rev": "deadbeef", "accel": "kvm"}), encoding="utf-8"
            )
            _write_state(root, "6.18.42")
            summary = build_summary(root)
        self.assertEqual(summary["campaign"]["git_rev"], "deadbeef")
        self.assertEqual(len(summary["versions"]), 1)

    def test_build_summary_tolerates_missing_campaign_json(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _write_state(root, "6.18.42")
            summary = build_summary(root)
        self.assertEqual(summary["campaign"], {})

    def test_render_main_table_marks_pass_fail_skip_distinctly(self):
        versions = [
            {
                "version": "6.18.42",
                "stages": {
                    "fetch": {"status": "pass"},
                    "smoke-v6": {"status": "skip"},
                },
            },
            {
                "version": "7.1.6",
                "stages": {"fetch": {"status": "fail", "detail": "no source at that URL"}},
            },
        ]
        table, footnotes = render_main_table(versions)
        self.assertIn("✅", table)
        self.assertIn("⏭", table)
        self.assertIn("❌", table)
        self.assertEqual(len(footnotes), 1)
        self.assertIn("no source at that URL", footnotes[0])

    def test_render_main_table_missing_stage_is_a_dash_not_a_crash(self):
        versions = [{"version": "6.1.180", "stages": {"fetch": {"status": "pass"}}}]
        table, footnotes = render_main_table(versions)
        self.assertIn("—", table)
        self.assertEqual(footnotes, [])

    def test_render_capabilities_table_bolds_false_values(self):
        versions = [
            {
                "version": "6.1.180",
                "capabilities": {"btf": True, "struct_ops": False, "notes": ["custom note"]},
            }
        ]
        table = render_capabilities_table(versions)
        self.assertIn("**false**", table)
        self.assertIn("custom note", table)

    def test_render_artifacts_table_truncates_long_hashes(self):
        versions = [
            {
                "version": "6.18.42",
                "artifacts": {"config_sha256": "a" * 64, "build_seconds": 3120},
            }
        ]
        table = render_artifacts_table(versions)
        self.assertIn("`aaaaaaaaaaaa…`", table)
        self.assertIn("3120", table)

    def test_render_report_flags_duplicate_vmlinux_h_sha_across_versions(self):
        summary = {
            "campaign": {},
            "versions": [
                {
                    "version": "6.12.101",
                    "overall": "pass",
                    "stages": {},
                    "capabilities": {},
                    "artifacts": {"vmlinux_h_sha256": "same" * 16},
                },
                {
                    "version": "6.18.42",
                    "overall": "pass",
                    "stages": {},
                    "capabilities": {},
                    "artifacts": {"vmlinux_h_sha256": "same" * 16},
                },
            ],
        }
        report = render_report(summary)
        self.assertIn("collision detected", report)
        self.assertIn("6.12.101", report)
        self.assertIn("6.18.42", report)

    def test_render_report_no_warning_when_hashes_all_distinct(self):
        summary = {
            "campaign": {},
            "versions": [
                {
                    "version": "6.12.101",
                    "overall": "pass",
                    "stages": {},
                    "capabilities": {},
                    "artifacts": {"vmlinux_h_sha256": "a" * 64},
                },
                {
                    "version": "6.18.42",
                    "overall": "pass",
                    "stages": {},
                    "capabilities": {},
                    "artifacts": {"vmlinux_h_sha256": "b" * 64},
                },
            ],
        }
        report = render_report(summary)
        self.assertNotIn("collision detected", report)

    def test_render_report_counts_pass_fail_running(self):
        summary = {
            "campaign": {},
            "versions": [
                {"version": "a", "overall": "pass", "stages": {}, "capabilities": {}},
                {"version": "b", "overall": "fail", "stages": {}, "capabilities": {}},
                {"version": "c", "overall": "running", "stages": {}, "capabilities": {}},
            ],
        }
        report = render_report(summary)
        self.assertIn("passed: 1", report)
        self.assertIn("failed: 1", report)
        self.assertIn("in progress: 1", report)

    def test_render_report_states_non_performance_banner(self):
        report = render_report({"campaign": {}, "versions": []})
        self.assertIn("not a performance validation", report)


if __name__ == "__main__":
    unittest.main()
