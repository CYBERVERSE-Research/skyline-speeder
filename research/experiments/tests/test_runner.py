import dataclasses
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

EXPERIMENT_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(EXPERIMENT_DIR))

from matrix_lib import (
    ModuleConfigSpec,
    _parse_module_config,
    _parse_redundancy,
    expand_runs,
    load_manifest,
)
from run_matrix import (
    case_attempts,
    communicate_or_terminate,
    execution_identity,
    load_analysis_invalid_case_ids,
    module_config_command,
    redundancy_command,
    successful_attempt,
    validate_execution_class,
)


class RunnerTests(unittest.TestCase):
    def test_module_config_command_sends_every_coefficient(self):
        # set-module-config replaces the whole set, and ssctl fills any flag
        # left out with its own built-in default, which follows the shipped
        # templates rather than the docs/04 coefficients the specs pin. A
        # field missing from the command would therefore run a case with a
        # value no manifest asked for. The two pacing switches are flags that
        # only appear when turned off.
        command = module_config_command(_parse_module_config({}))
        for field in dataclasses.fields(ModuleConfigSpec):
            if field.name in ("prr_pacing_enabled", "auto_pacing_enabled"):
                continue
            flag = "--" + field.name.replace("_", "-")
            with self.subTest(field=field.name):
                self.assertIn(flag, command)

    def test_module_config_command_pins_the_docs04_cwnd_floor(self):
        command = module_config_command(_parse_module_config({}))
        index = command.index("--min-cwnd-packets")
        self.assertEqual(command[index + 1], "4")
        command = module_config_command(_parse_module_config({"min_cwnd_packets": 32}))
        index = command.index("--min-cwnd-packets")
        self.assertEqual(command[index + 1], "32")

    def test_redundancy_is_off_unless_a_profile_asks_for_it(self):
        # The installed template turns first-flight redundancy on, and a reset
        # would restore that; every docs/04 case ran without copies, so a
        # profile without [profiles.redundancy] must be sent --disable.
        self.assertEqual(
            redundancy_command(None), ["sudo", "ssctl", "set-redundancy", "--disable"]
        )
        self.assertEqual(
            redundancy_command(_parse_redundancy({})),
            ["sudo", "ssctl", "set-redundancy", "--disable"],
        )
        command = redundancy_command(
            _parse_redundancy({"enabled": True, "first_kib": 32, "delay_ms": 5})
        )
        self.assertEqual(
            command,
            ["sudo", "ssctl", "set-redundancy", "--first-kib", "32", "--delay-ms", "5"],
        )

    def test_execution_identity_ignores_mutable_runtime_fields(self):
        first = {
            "execution_class": "tcg-validation",
            "performance_valid": False,
            "acceleration": "tcg",
            "qemu": {"version": "8.2"},
            "host_kernel": "6.8",
            "resources": {"vcpus": 2},
            "server": {"image": "/images/server.qcow2", "pid": 1},
            "client": {"image": "/images/client.qcow2", "pid": 2},
        }
        second = json.loads(json.dumps(first))
        second["server"]["pid"] = 100
        second["client"]["pid"] = 200
        second["started_at"] = "later"
        self.assertEqual(execution_identity(first), execution_identity(second))

    def test_execution_class_mismatch_is_rejected(self):
        manifest = load_manifest(
            EXPERIMENT_DIR / "manifests" / "kernel-compat-smoke-tcg.toml"
        )
        with self.assertRaisesRegex(RuntimeError, "execution class mismatch"):
            validate_execution_class(
                manifest,
                {"execution_class": "formal-kvm", "performance_valid": True},
            )

    def test_resume_finds_latest_successful_attempt(self):
        manifest = load_manifest(
            EXPERIMENT_DIR / "manifests" / "kernel-compat-smoke-tcg.toml"
        )
        case = expand_runs(manifest)[0]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for attempt, valid in ((1, False), (2, True)):
                path = root / f"{case.case_id}--attempt{attempt:02d}"
                path.mkdir()
                (path / "status.json").write_text(
                    json.dumps({"valid": valid}), encoding="utf-8"
                )
            attempts = case_attempts(root, case)
            self.assertEqual([number for number, _ in attempts], [1, 2])
            self.assertTrue(successful_attempt(attempts).name.endswith("attempt02"))

    def test_load_analysis_invalid_case_ids_selects_non_true_rows(self):
        with tempfile.TemporaryDirectory() as directory:
            csv_path = Path(directory) / "runs.csv"
            csv_path.write_text(
                "case_id,valid\n"
                "a--rtt100,True\n"
                "b--rtt200,False\n"
                "c--rtt300,\n",
                encoding="utf-8",
            )
            self.assertEqual(
                load_analysis_invalid_case_ids(csv_path),
                {"b--rtt200", "c--rtt300"},
            )

    def test_communicate_or_terminate_returns_output_for_finished_process(self):
        # text=True matches Remote.popen()'s real construction -- the
        # actual server_process/metrics_process/perf_process objects this
        # is used on all decode to str, which write_text() requires.
        process = subprocess.Popen(
            ["echo", "hello"],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        code, stdout, stderr = communicate_or_terminate(process, 5)
        self.assertEqual(code, 0)
        self.assertEqual(stdout, "hello\n")
        self.assertEqual(stderr, "")

    def test_communicate_or_terminate_terminates_a_hung_process(self):
        process = subprocess.Popen(
            ["sleep", "60"],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        communicate_or_terminate(process, 0.2)
        # terminate_process() inside communicate_or_terminate must have
        # actually reaped it -- a case that fails early must not leave
        # orphaned guest-side processes behind for the next case to trip
        # over.
        self.assertIsNotNone(process.poll())


if __name__ == "__main__":
    unittest.main()
