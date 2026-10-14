#!/usr/bin/env python3
"""Run Deploy's actual input/deploy shell steps with the real CVM wrapper and a stub CLI."""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


def step_run(name):
    workflow = (ROOT / ".github/workflows/deploy.yml").read_text()
    match = re.search(r"^      - name: " + re.escape(name) + r"\n"
                      r"(?:(?!^      - ).*\n)*?        run: \|\n"
                      r"((?:          .*\n|\n)+)", workflow, re.MULTILINE)
    if match is None:
        raise AssertionError(f"No shell step found: {name}")
    return "\n".join(line[10:] for line in match[1].splitlines())


class DeployWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="deploy-workflow-test-")
        self.addCleanup(self.directory.cleanup)
        self.tmp = Path(self.directory.name)
        self.kit = self.tmp / "kit"
        deploy = self.kit / "deploy"
        deploy.mkdir(parents=True)
        for script in ("phala-cvm.sh", "deadline.sh", "preflight-phala.sh"):
            shutil.copy2(ROOT / "deploy" / script, deploy)
        (deploy / "upgrade-monitor.py").write_text("# No network sampling in this workflow test.\n")
        cli = deploy / "phala"
        cli.write_text('''#!/usr/bin/env bash
set -euo pipefail
jq -nc '$ARGS.positional' --args -- "$@" >>"$STUB_LOG"
case "$1" in
    deploy) echo '{"success":true,"vm_uuid":"cvm-1","app_id":"app-1"}' ;;
    api)
        if [[ "$2" == */available-os-images ]]; then
            printf '%s\\n' "$STUB_AVAILABLE_IMAGES"
        else
            [[ "${STUB_OS_STATUS:-0}" == 0 ]] || exit "$STUB_OS_STATUS"
            touch "$STUB_OS_STARTED"
        fi ;;
    cvms)
        busy=false
        if [[ -f "$STUB_OS_STARTED" ]]; then
            busy=true
            rm "$STUB_OS_STARTED"
        fi
        jq -nc --argjson busy "$busy" '{os:{name:"dstack-0.5.12"},status:"running",in_progress:$busy,
            compose_hash:"ab12",node_info:{id:26,name:"prod5",region:"US-WEST-1"}}' ;;
    *) exit 64 ;;
esac
''')
        cli.chmod(0o755)
        pause = deploy / "instance-pause.sh"
        pause.write_text('''#!/usr/bin/env bash
jq -nc '["maintenance"] + $ARGS.positional' --args -- "$@" >>"$STUB_LOG"
[[ "${STUB_PAUSE_STATUS:-0}" == 0 ]] || exit "$STUB_PAUSE_STATUS"
echo '{"paused_scopes":["mutations"]}'
''')
        pause.chmod(0o755)
        preflight = deploy / "preflight.sh"
        preflight.write_text("#!/bin/sh\nexit 0\n")
        preflight.chmod(0o755)
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir()
        curl = bin_dir / "curl"
        curl.write_text('''#!/usr/bin/env bash
jq -nc '["health"] + $ARGS.positional' --args -- "$@" >>"$STUB_LOG"
''')
        curl.chmod(0o755)
        sleep = bin_dir / "sleep"
        sleep.write_text("#!/bin/sh\nexit 0\n")
        sleep.chmod(0o755)
        self.work = self.tmp / "work"
        (self.work / "release").mkdir(parents=True)
        (self.work / "release/images.json").write_text('{"phala-pay":"test-image"}')
        self.set_current_image("dstack-0.5.12")
        (self.tmp / "env-file").write_text("SENTRY_DSN=\n")
        self.env = {**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}",
                    "KIT": str(self.kit), "WORK": str(self.work), "RUNNER_TEMP": str(self.tmp),
                    "STUB_LOG": str(self.tmp / "cli.jsonl"), "GITHUB_ENV": str(self.tmp / "env"),
                    "STUB_OS_STARTED": str(self.tmp / "os-started"),
                    "GITHUB_STEP_SUMMARY": str(self.tmp / "summary"),
                    "PHALA_CLOUD_API_KEY": "test-key", "PHALA_WORKSPACE": "test-workspace",
                    "PHALA_NODE_ID": "", "TOPUP_CVM_ID": "", "STAGING_PRODUCT_CVM_ID": "",
                    "VERSION": "v0.10.4", "DEPLOY_ENVIRONMENT": "production", "TARGET": "topup",
                    "MODE": "provision", "ENVIRONMENT_DIR": "production/topup",
                    "CVM_NAME": "test-cvm", "CVM_ID_VARIABLE": "TOPUP_CVM_ID",
                    "EXISTING_CVM_ID": "cvm-1", "COMPOSE": str(self.tmp / "compose.yml"),
                    "ENV_FILE": str(self.tmp / "env-file"), "INSTANCE_TYPE": "tdx.medium",
                    "DSTACK_OS_IMAGE": "dstack-0.5.12", "IMAGES": "phala-pay",
                    "PUBLIC_URL": "https://example.invalid", "GITHUB_RUN_ID": "1",
                    "GITHUB_RUN_ATTEMPT": "1", "GITHUB_OUTPUT": str(self.tmp / "output"),
                    "PREFLIGHT": str(preflight), "STUB_AVAILABLE_IMAGES": json.dumps([
                        {"version": [0, 5, 12], "prod": {"name": "dstack-0.5.12", "enabled": True}}
                    ]),
                    "SENTRY_DSN": "",
                    "GITHUB_SHA": "test-sha", "GITHUB_SERVER_URL": "https://github.com",
                    "RELEASE_REPOSITORY": "Phala-Network/phala-pay"}

    def set_current_image(self, image):
        (self.work / "cvm-before.json").write_text(json.dumps({"os": {"name": image}}))

    def calls(self):
        return [json.loads(line) for line in (self.tmp / "cli.jsonl").read_text().splitlines()]

    def run_step(self, name):
        return subprocess.run(["bash", "-eo", "pipefail", "-c", step_run(name)],
                              env=self.env, capture_output=True, text=True, timeout=10)

    def test_node_validation(self):
        for node in ("", "26", "0", "026"):
            with self.subTest(node=node):
                self.env["PHALA_NODE_ID"] = node
                result = self.run_step("Check inputs and Environment settings")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        for node in ("prod5", "-1", "2.6", " 26", "26 ", "26\n", "26; echo unsafe"):
            with self.subTest(node=node):
                self.env["PHALA_NODE_ID"] = node
                result = self.run_step("Check inputs and Environment settings")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("PHALA_NODE_ID must be empty (auto) or digits", result.stdout)

    def test_node_selection_and_summary(self):
        for mode, node in (("provision", "26"), ("provision", ""), ("upgrade", "26")):
            with self.subTest(mode=mode, node=node):
                self.env.update(MODE=mode, PHALA_NODE_ID=node)
                for path in (self.tmp / "cli.jsonl", self.tmp / "summary"):
                    path.unlink(missing_ok=True)
                result = self.run_step("Deploy (${{ inputs.mode }}) and wait for the CVM")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                calls = [json.loads(line) for line in (self.tmp / "cli.jsonl").read_text().splitlines()]
                deployment = next(args for args in calls if args[0] == "deploy")
                if mode == "provision" and node:
                    self.assertEqual(deployment[deployment.index("--node-id") + 1], "26")
                else:
                    self.assertNotIn("--node-id", deployment)
                if mode == "upgrade":
                    self.assertNotIn("-e", deployment)
                    self.assertEqual(deployment[deployment.index("--cvm-id") + 1], "cvm-1")
                self.assertFalse(any(args[0] == "api" for args in calls))
                summary = (self.tmp / "summary").read_text()
                requested = f"`{node or 'auto'}`" if mode == "provision" else "unchanged (upgrade)"
                self.assertIn(f"| Requested node | {requested} |", summary)
                self.assertIn("| CVM node | 26 (prod5, US-WEST-1) |", summary)

    def test_os_image_then_health_then_fresh_lease_then_compose(self):
        self.env["MODE"] = "upgrade"
        self.set_current_image("dstack-0.5.9")
        result = self.run_step("Pause new mutations before upgrading")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        result = self.run_step("Deploy (${{ inputs.mode }}) and wait for the CVM")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        commands = [args[0] for args in self.calls()]
        self.assertEqual(commands, ["maintenance", "api", "cvms", "cvms", "health", "maintenance",
                                    "deploy", "cvms", "health"])
        leases = [args for args in self.calls() if args[0] == "maintenance"]
        self.assertEqual(leases[0], leases[1])

    def test_product_os_image_change_without_maintenance(self):
        self.env.update(MODE="upgrade", TARGET="product")
        self.set_current_image("dstack-0.5.9")
        result = self.run_step("Deploy (${{ inputs.mode }}) and wait for the CVM")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual([args[0] for args in self.calls()],
                         ["api", "cvms", "cvms", "health", "deploy", "cvms", "health"])

    def test_os_image_or_fresh_lease_failure_prevents_compose_deploy(self):
        self.env["MODE"] = "upgrade"
        self.set_current_image("dstack-0.5.9")
        for failure in ("STUB_OS_STATUS", "STUB_PAUSE_STATUS"):
            with self.subTest(failure=failure):
                (self.tmp / "cli.jsonl").unlink(missing_ok=True)
                self.env[failure] = "42"
                result = self.run_step("Deploy (${{ inputs.mode }}) and wait for the CVM")
                self.assertEqual(result.returncode, 42, result.stdout + result.stderr)
                self.assertFalse(any(args[0] == "deploy" for args in self.calls()))
                del self.env[failure]

    def test_upgrade_preflight_uses_cvm_specific_allowlist(self):
        self.env["MODE"] = "upgrade"
        result = self.run_step("Preflight (read-only)")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.calls(), [["api", "/cvms/cvm-1/available-os-images"]])
        for images in ([], [{"version": [0, 5, 12], "prod": None}],
                       [{"version": [0, 5, 12], "prod": {"name": "dstack-0.5.12", "enabled": False}}],
                       [{"version": [0, 5, 9], "prod": {"name": "dstack-0.5.9", "enabled": True}}]):
            with self.subTest(images=images):
                self.env["STUB_AVAILABLE_IMAGES"] = json.dumps(images)
                result = self.run_step("Preflight (read-only)")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("does not include enabled production image", result.stderr)


if __name__ == "__main__":
    unittest.main()
