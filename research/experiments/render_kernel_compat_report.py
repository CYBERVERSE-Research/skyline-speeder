#!/usr/bin/env python3
"""Renders the multi-kernel compatibility pipeline's report.

Input: a campaign root directory written by infra/kernel/run-compat-pipeline.sh,
shaped like:
    <campaign_root>/campaign.json                 -- environment freeze block
    <campaign_root>/versions/<version>/state.json -- one per kernel version

state.json schema (each stage's value is {"status": "pass"|"fail"|"skip",
"detail": str, optional "log": str-path-relative-to-version-dir}):
    {
      "version": "6.18.42",
      "overall": "pass" | "fail" | "running",
      "identity": {"repo_rev": "...", "repo_dirty": bool, ...},
      "stages": {"fetch": {...}, "build": {...}, "boot": {...},
                 "kernel-install": {...}, "uname-gate": {...},
                 "bpf-compile": {...}, "skyline-deploy": {...},
                 "smoke-static": {...}, "smoke-v4": {...}, "smoke-v6": {...},
                 "teardown": {...}},
      "capabilities": {"btf": bool, "bpffs": bool, "cgroup_v2": bool,
                        "fq_available": bool, "struct_ops": bool,
                        "rack_reo_hook": bool, "fallback_cc_available": bool,
                        "notes": [str, ...]},
      "artifacts": {"config_sha256": str, "vmlinux_sha256": str,
                    "vmlinux_h_sha256": str, "bpf_object_sha256": {...},
                    "build_seconds": number},
      "failure": {"stage": str, "message": str, "log": str} -- only if overall=="fail"
    }

This module is a pure function of its input directory -- it does not build,
deploy, or SSH anywhere. Output directory must not already exist (same
"never overwrite results" convention as analyze_results.py).
"""
import argparse
import json
import sys
from pathlib import Path

STAGE_ORDER = [
    "fetch",
    "build",
    "overlay",
    "boot",
    "kernel-install",
    "uname-gate",
    "bpf-compile",
    "skyline-deploy",
    "smoke-static",
    "smoke-v4",
    "smoke-v6",
    "teardown",
]

CAPABILITY_KEYS = [
    "btf",
    "bpffs",
    "cgroup_v2",
    "fq_available",
    "struct_ops",
    "rack_reo_hook",
    "fallback_cc_available",
]

STATUS_ICON = {"pass": "✅", "fail": "❌", "skip": "⏭", None: "—"}


def load_campaign_versions(campaign_root):
    """Reads every versions/*/state.json under campaign_root, sorted by
    version string as they appear on disk (caller-controlled order via
    directory creation order is NOT relied upon -- sorted() here is the
    single source of ordering, so re-running the renderer against the same
    campaign_root always produces the same table row order)."""
    versions_dir = campaign_root / "versions"
    versions = []
    if not versions_dir.is_dir():
        return versions
    for entry in sorted(versions_dir.iterdir()):
        state_path = entry / "state.json"
        if not state_path.is_file():
            continue
        try:
            versions.append(json.loads(state_path.read_text(encoding="utf-8")))
        except json.JSONDecodeError as error:
            versions.append(
                {
                    "version": entry.name,
                    "overall": "fail",
                    "stages": {},
                    "capabilities": {},
                    "failure": {
                        "stage": "state-file",
                        "message": f"state.json is not valid JSON: {error}",
                        "log": str(state_path),
                    },
                }
            )
    return versions


def build_summary(campaign_root):
    campaign_path = campaign_root / "campaign.json"
    campaign = {}
    if campaign_path.is_file():
        campaign = json.loads(campaign_path.read_text(encoding="utf-8"))
    return {
        "campaign": campaign,
        "versions": load_campaign_versions(campaign_root),
    }


def _stage_cell(stages, name):
    stage = stages.get(name)
    if stage is None:
        return STATUS_ICON[None]
    return STATUS_ICON.get(stage.get("status"), "?")


def _footnotes_for_version(version_entry):
    notes = []
    for stage_name, stage in version_entry.get("stages", {}).items():
        if stage.get("status") == "fail":
            detail = stage.get("detail", "")
            log = stage.get("log", "")
            notes.append(
                f"`{version_entry.get('version', '?')}` / `{stage_name}`: {detail}"
                + (f" (see `{log}`)" if log else "")
            )
    return notes


def render_main_table(versions):
    header = ["Version"] + STAGE_ORDER
    lines = [
        "| " + " | ".join(header) + " |",
        "|" + "---|" * len(header),
    ]
    footnotes = []
    for entry in versions:
        stages = entry.get("stages", {})
        row = [f"`{entry.get('version', '?')}`"]
        row += [_stage_cell(stages, name) for name in STAGE_ORDER]
        lines.append("| " + " | ".join(row) + " |")
        footnotes += _footnotes_for_version(entry)
    return "\n".join(lines), footnotes


def render_capabilities_table(versions):
    header = ["Version"] + CAPABILITY_KEYS + ["notes"]
    lines = [
        "| " + " | ".join(header) + " |",
        "|" + "---|" * len(header),
    ]
    for entry in versions:
        caps = entry.get("capabilities", {})
        row = [f"`{entry.get('version', '?')}`"]
        for key in CAPABILITY_KEYS:
            value = caps.get(key)
            row.append("—" if value is None else ("true" if value else "**false**"))
        notes = caps.get("notes") or []
        row.append("; ".join(notes) if notes else "")
        lines.append("| " + " | ".join(row) + " |")
    return "\n".join(lines)


def render_artifacts_table(versions):
    header = ["Version", "config sha256", "vmlinux sha256", "vmlinux.h sha256", "build_seconds"]
    lines = [
        "| " + " | ".join(header) + " |",
        "|" + "---|" * len(header),
    ]
    for entry in versions:
        artifacts = entry.get("artifacts", {})

        def short(key):
            value = artifacts.get(key)
            return f"`{value[:12]}…`" if value else "—"

        lines.append(
            "| "
            + " | ".join(
                [
                    f"`{entry.get('version', '?')}`",
                    short("config_sha256"),
                    short("vmlinux_sha256"),
                    short("vmlinux_h_sha256"),
                    str(artifacts.get("build_seconds", "—")),
                ]
            )
            + " |"
        )
    return "\n".join(lines)


def render_report(summary):
    campaign = summary.get("campaign", {})
    versions = summary.get("versions", [])
    passed = sum(1 for v in versions if v.get("overall") == "pass")
    failed = sum(1 for v in versions if v.get("overall") == "fail")
    running = sum(1 for v in versions if v.get("overall") == "running")

    main_table, footnotes = render_main_table(versions)

    # vmlinux.h sha256 must be pairwise distinct across versions -- a repeat
    # means the Makefile timestamp trap (see infra/kernel/run-compat-pipeline.sh's
    # design notes) reused a stale header instead of regenerating it, and
    # every downstream check for that version is suspect regardless of what
    # it reported.
    vmlinux_h_shas = {}
    for entry in versions:
        sha = entry.get("artifacts", {}).get("vmlinux_h_sha256")
        if sha:
            vmlinux_h_shas.setdefault(sha, []).append(entry.get("version", "?"))
    stale_header_warning = ""
    duplicated = {sha: vs for sha, vs in vmlinux_h_shas.items() if len(vs) > 1}
    if duplicated:
        lines = [
            f"  - `{sha[:12]}…` shared by {', '.join(vs)}" for sha, vs in duplicated.items()
        ]
        stale_header_warning = (
            "\n**⚠️ vmlinux.h sha256 collision detected** -- two or more versions "
            "share the same generated header, meaning at least one of them was "
            "built against the wrong kernel's BTF (see the Makefile timestamp "
            "trap noted in run-compat-pipeline.sh's design comments). Every "
            "check for the affected versions needs to be re-run after "
            "confirming the header is regenerated fresh:\n" + "\n".join(lines) + "\n"
        )

    sections = [
        "# Multi-kernel-version compatibility validation report",
        "",
        "**This round is a compatibility smoke validation, not a performance validation.** "
        "`performance_valid=true` (if a case's manifest is marked that way) is only a schema "
        "constraint of the `formal-kvm` execution_class; no number in this report may be "
        "interpreted as a conclusion about algorithm gains: no throughput/goodput comparison, "
        "no CV threshold, no B1/B2 neutrality, no multi-CC performance matrix, no "
        "hyperparameter sweep, and not a validation at the level of the target environment "
        "grid either.",
        "",
        f"Total versions: {len(versions)}, passed: {passed}, failed: {failed}, "
        f"in progress: {running}",
        stale_header_warning,
        "## Frozen environment",
        "",
        "| Field | Value |",
        "|---|---|",
    ]
    env_fields = [
        ("git_rev", campaign.get("git_rev", "—")),
        ("git_dirty", campaign.get("git_dirty", "—")),
        ("accel", campaign.get("accel", "—")),
        ("qemu_version", campaign.get("qemu_version", "—")),
        ("skyline_tc_version", campaign.get("skyline_tc_version", "—")),
        ("host_cpu", campaign.get("host_cpu", "—")),
        ("host_memory_mib", campaign.get("host_memory_mib", "—")),
        ("bootstrap_overlay", campaign.get("bootstrap_overlay", "—")),
    ]
    for key, value in env_fields:
        sections.append(f"| {key} | {value} |")

    sections += [
        "",
        "## Version × checks",
        "",
        main_table,
        "",
    ]
    if footnotes:
        sections.append("### Failure details")
        sections.append("")
        sections += [f"- {note}" for note in footnotes]
        sections.append("")

    sections += [
        "## Version × capabilities",
        "",
        render_capabilities_table(versions),
        "",
        "## Build artifacts",
        "",
        render_artifacts_table(versions),
        "",
        "## Known limitations",
        "",
        "- No throughput/goodput comparison, CV threshold, B1/B2 neutrality, multi-CC "
        "performance matrix or hyperparameter sweep, and not a validation at the level of "
        "the target environment grid either.",
        "- `rack_reo_hook` is expected to always be false (it probes for a custom kernel "
        "patch that upstream kernels do not have in the first place): it is only recorded, "
        "not used as a failure criterion; only a change in its value across versions is "
        "worth noticing.",
        "- The IPv6 smoke test has two levels: L1 (connectivity + no crash) always runs; "
        "L2 (marking correctness) depends on whether IPv6 support had landed at the time; "
        "before it lands it is recorded as skip, not fail.",
    ]
    return "\n".join(sections) + "\n"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("campaign_root", type=Path)
    parser.add_argument("output_dir", type=Path)
    args = parser.parse_args()

    if not args.campaign_root.is_dir():
        print(f"campaign root does not exist: {args.campaign_root}", file=sys.stderr)
        return 1
    args.output_dir.mkdir(parents=True, exist_ok=False)

    summary = build_summary(args.campaign_root)
    (args.output_dir / "summary.json").write_text(
        json.dumps(summary, indent=2, sort_keys=True), encoding="utf-8"
    )
    (args.output_dir / "report.md").write_text(render_report(summary), encoding="utf-8")
    print(f"rendered report for {len(summary['versions'])} version(s) into {args.output_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
