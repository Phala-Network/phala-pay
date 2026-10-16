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
        for script in ("phala-cvm.sh", "deadline.sh", "preflight-phala.sh", "compose-policy.jq", "phala-cloud-pre-launch.sh"):
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
        elif [[ "$2" == */compose_file/provision ]]; then
            jq -nc '{compose_hash: ("ab" * 32), app_id: null}'
        elif [[ "$2" == */compose_file ]]; then
            if [[ "${3:-}" == -X ]]; then
                touch "$STUB_COMPOSE_COMMITTED"
            else
                echo '{"docker_compose_file":"old", "allowed_envs":["SENTRY_DSN"]}'
            fi
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
        hash=$(printf 'cd%.0s' {1..32})
        [[ ! -f "$STUB_COMPOSE_COMMITTED" ]] || hash=$(printf 'ab%.0s' {1..32})
        jq -nc --arg hash "$hash" --argjson busy "$busy" '{os:{name:"dstack-0.6.1"},status:"running",in_progress:$busy,
            compose_hash:$hash,node_info:{id:26,name:"prod5",region:"US-WEST-1"}}' ;;
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
        (self.tmp / "compose.yml").write_text("services: {}\n")
        self.set_current_image("dstack-0.6.1")
        (self.tmp / "env-file").write_text("SENTRY_DSN=\n")
        self.env = {**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}",
                    "KIT": str(self.kit), "WORK": str(self.work), "RUNNER_TEMP": str(self.tmp),
                    "STUB_LOG": str(self.tmp / "cli.jsonl"), "GITHUB_ENV": str(self.tmp / "env"),
                    "STUB_OS_STARTED": str(self.tmp / "os-started"),
                    "STUB_COMPOSE_COMMITTED": str(self.tmp / "compose-committed"),
                    "VARIANT": "service",
                    "GITHUB_STEP_SUMMARY": str(self.tmp / "summary"),
                    "PHALA_CLOUD_API_KEY": "test-key", "PHALA_WORKSPACE": "test-workspace",
                    "PHALA_NODE_ID": "", "TOPUP_CVM_ID": "", "STAGING_PRODUCT_CVM_ID": "",
                    "VERSION": "v0.10.4", "DEPLOY_ENVIRONMENT": "production", "TARGET": "topup",
                    "MODE": "provision", "ENVIRONMENT_DIR": "production/topup",
                    "CVM_NAME": "test-cvm", "CVM_ID_VARIABLE": "TOPUP_CVM_ID",
                    "EXISTING_CVM_ID": "cvm-1", "COMPOSE": str(self.tmp / "compose.yml"),
                    "ENV_FILE": str(self.tmp / "env-file"), "INSTANCE_TYPE": "tdx.medium",
                    "DSTACK_OS_IMAGE": "dstack-0.6.1", "IMAGES": "phala-pay",
                    "PUBLIC_URL": "https://example.invalid", "GITHUB_RUN_ID": "1",
                    "GITHUB_RUN_ATTEMPT": "1", "GITHUB_OUTPUT": str(self.tmp / "output"),
                    "PREFLIGHT": str(preflight), "STUB_AVAILABLE_IMAGES": json.dumps([
                        {"version": [0, 6, 1], "prod": {"name": "dstack-0.6.1", "enabled": True}}
                    ]),
                    "SENTRY_DSN": "",
                    "GITHUB_SHA": "test-sha", "GITHUB_SERVER_URL": "https://github.com",
                    "RELEASE_REPOSITORY": "Phala-Network/phala-pay"}

    def set_current_image(self, image):
        (self.work / "cvm-before.json").write_text(json.dumps({"os": {"name": image}, "app_id": "app-1"}))

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
        for mode, node, variant in (("provision", "26", "service"), ("provision", "", "service"),
                                    ("upgrade", "26", "service"), ("upgrade", "26", "product")):
            with self.subTest(mode=mode, node=node, variant=variant):
                self.env.update(MODE=mode, PHALA_NODE_ID=node, VARIANT=variant,
                                TARGET="topup" if variant == "service" else "product")
                for path in (self.tmp / "cli.jsonl", self.tmp / "summary", self.tmp / "compose-committed"):
                    path.unlink(missing_ok=True)
                result = self.run_step("Deploy (${{ inputs.mode }}) and wait for the CVM")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                calls = [json.loads(line) for line in (self.tmp / "cli.jsonl").read_text().splitlines()]
                if mode == "provision":
                    deployment = next(args for args in calls if args[0] == "deploy")
                    if node:
                        self.assertEqual(deployment[deployment.index("--node-id") + 1], "26")
                    else:
                        self.assertNotIn("--node-id", deployment)
                    self.assertFalse(any(args[0] == "api" for args in calls))
                else:
                    self.assertFalse(any(args[0] == "deploy" for args in calls))
                    updates = [args for args in calls if args[0] == "api"]
                    self.assertEqual([args[1] for args in updates], ["/cvms/cvm-1/compose_file",
                                     "/cvms/cvm-1/compose_file/provision", "/cvms/cvm-1/compose_file"])
                    for update in updates:
                        self.assertNotIn("--node-id", update)
                        self.assertNotIn("-e", update)
                    self.assert_policy_request(self.work / "deploy.json.request.json")
                summary = (self.tmp / "summary").read_text()
                requested = f"`{node or 'auto'}`" if mode == "provision" else "unchanged (upgrade)"
                self.assertIn(f"| Requested node | {requested} |", summary)
                self.assertIn("| CVM node | 26 (prod5, US-WEST-1) |", summary)

    def test_product_upgrade_rerun_waits_for_current_hash_without_commit(self):
        self.env.update(MODE="upgrade", TARGET="product", VARIANT="product")
        result = self.run_step("Deploy (${{ inputs.mode }}) and wait for the CVM")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        (self.tmp / "cli.jsonl").unlink()
        result = self.run_step("Deploy (${{ inputs.mode }}) and wait for the CVM")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("unchanged; nothing to commit", result.stderr)
        self.assertEqual([args[0] for args in self.calls()], ["cvms", "api", "api", "cvms", "health"])
        updates = [args for args in self.calls() if args[0] == "api"]
        self.assertEqual([args[1] for args in updates], ["/cvms/cvm-1/compose_file",
                         "/cvms/cvm-1/compose_file/provision"])
        self.assertEqual(json.loads((self.work / "cvm.json").read_text())["compose_hash"], "ab" * 32)

    def test_os_image_then_health_then_fresh_lease_then_compose(self):
        self.env["MODE"] = "upgrade"
        self.set_current_image("dstack-0.5.12")
        result = self.run_step("Pause new mutations before upgrading")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        result = self.run_step("Deploy (${{ inputs.mode }}) and wait for the CVM")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        commands = [args[0] for args in self.calls()]
        self.assertEqual(commands, ["maintenance", "api", "cvms", "cvms", "health", "maintenance",
                                    "cvms", "api", "api", "api", "cvms", "health"])
        probes = [args for args in self.calls() if args[0] == "health"]
        self.assertTrue(all(args[-1] == f"{self.env['PUBLIC_URL']}/healthz" for args in probes))
        leases = [args for args in self.calls() if args[0] == "maintenance"]
        self.assertEqual(leases[0], leases[1])

    def test_product_os_image_change_without_maintenance(self):
        self.env.update(MODE="upgrade", TARGET="product", VARIANT="product")
        self.set_current_image("dstack-0.5.9")
        result = self.run_step("Deploy (${{ inputs.mode }}) and wait for the CVM")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual([args[0] for args in self.calls()],
                         ["api", "cvms", "cvms", "health", "cvms", "api", "api", "api", "cvms", "health"])

    def test_os_image_or_fresh_lease_failure_prevents_compose_deploy(self):
        self.env["MODE"] = "upgrade"
        self.set_current_image("dstack-0.5.9")
        for failure in ("STUB_OS_STATUS", "STUB_PAUSE_STATUS"):
            with self.subTest(failure=failure):
                (self.tmp / "cli.jsonl").unlink(missing_ok=True)
                self.env[failure] = "42"
                result = self.run_step("Deploy (${{ inputs.mode }}) and wait for the CVM")
                self.assertEqual(result.returncode, 42, result.stdout + result.stderr)
                self.assertFalse(any(args[0] == "deploy" or "compose_file" in args[1] for args in self.calls()))
                del self.env[failure]

    def assert_policy_request(self, path):
        body = json.loads(path.read_text())
        self.assertEqual(body["port_policy"], {
            "ports": [{"port": 443, "pp": self.env["VARIANT"] == "service"}], "restrict_mode": False})
        self.assertEqual(body["allowed_envs"], ["SENTRY_DSN"])
        for key in ("encrypted_env", "env_keys", "update_env_vars"):
            self.assertNotIn(key, body)

    def test_provision_followup_commits_gateway_and_port_policy_for_both_targets(self):
        render = self.kit / "deploy/render.sh"
        render.write_text("#!/bin/sh\nprintf 'services: {}\\n'\n")
        render.chmod(0o755)
        for target, variant in (("topup", "service"), ("product", "product")):
            with self.subTest(target=target):
                self.env.update(TARGET=target, VARIANT=variant, CVM_ID="cvm-1")
                (self.tmp / "compose-committed").unlink(missing_ok=True)
                (self.work / "cvm.json").write_text(json.dumps({"gateway": {"base_domain": "example.invalid"},
                                                               "compose_hash": "ab12"}))
                result = self.run_step("Set the values known only after provisioning")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assert_policy_request(self.work / "deploy-public-url.json.request.json")

    def test_upgrade_preflight_uses_cvm_specific_allowlist(self):
        self.env["MODE"] = "upgrade"
        result = self.run_step("Preflight (read-only)")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.calls(), [["api", "/cvms/cvm-1/available-os-images"]])
        for images in ([], [{"version": [0, 6, 1], "prod": None}],
                       [{"version": [0, 6, 1], "prod": {"name": "dstack-0.6.1", "enabled": False}}],
                       [{"version": [0, 5, 9], "prod": {"name": "dstack-0.5.9", "enabled": True}}],
                       [{"version": [0, 5, 12], "prod": {"name": "dstack-0.5.12", "enabled": True}}]):
            with self.subTest(images=images):
                self.env["STUB_AVAILABLE_IMAGES"] = json.dumps(images)
                result = self.run_step("Preflight (read-only)")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("does not include enabled production image", result.stderr)


if __name__ == "__main__":
    unittest.main()
