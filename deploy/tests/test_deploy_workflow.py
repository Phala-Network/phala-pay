#!/usr/bin/env python3
"""Run Deploy's actual input/deploy shell steps with the real CVM wrapper and a stub CLI."""
import hashlib
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
        inline = re.search(r"^      - name: " + re.escape(name) + r"\n"
                           r"(?:(?!^      - ).*\n)*?        run: '(.*)'\n", workflow, re.MULTILINE)
        if inline is not None:
            return inline[1].replace("''", "'")
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
            jq -nc --arg hash "${STUB_TARGET_HASH:-$(printf 'ab%.0s' {1..32})}" '{compose_hash:$hash, app_id:null}'
        elif [[ "$2" == */compose_file ]]; then
            if [[ "${3:-}" == -X ]]; then
                touch "$STUB_COMPOSE_COMMITTED"
            else
                if [[ -n "${STUB_SOURCE_APP:-}" ]]; then cat "$STUB_SOURCE_APP"
                else echo '{"docker_compose_file":"old", "allowed_envs":["SENTRY_DSN"]}'; fi
            fi
        else
            [[ "${STUB_OS_STATUS:-0}" == 0 ]] || exit "$STUB_OS_STATUS"
            touch "$STUB_OS_STARTED"
        fi ;;
    cvms)
        if [[ "$2" == attestation ]]; then
            if [[ -f "$STUB_COMPOSE_COMMITTED" ]]; then cat "$STUB_TARGET_ATTESTATION"
            else cat "$STUB_SOURCE_ATTESTATION"; fi
            exit 0
        fi
        busy=false
        if [[ -f "$STUB_OS_STARTED" ]]; then
            busy=true
            rm "$STUB_OS_STARTED"
        fi
        hash=${STUB_SOURCE_HASH:-$(printf 'cd%.0s' {1..32})}
        [[ ! -f "$STUB_COMPOSE_COMMITTED" ]] || hash=${STUB_TARGET_HASH:-$(printf 'ab%.0s' {1..32})}
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
if [[ "${*: -1}" == */v1/attestation/info ]]; then
    jq -nc '["info"] + $ARGS.positional' --args -- "$@" >>"$STUB_LOG"
    [[ "${STUB_INFO_STATUS:-0}" == 0 ]] || exit "$STUB_INFO_STATUS"
    echo '{"vm_config":"fixture-vm-config"}'
    exit 0
fi
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
                    "PHALA_NODE": "", "PHALA_NODE_ID": "", "TOPUP_CVM_ID": "", "STAGING_PRODUCT_CVM_ID": "",
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
        for node in ("", "prod5", "usw5", "us-west-1", "Prod5"):
            with self.subTest(node=node):
                self.env["PHALA_NODE"] = node
                result = self.run_step("Check inputs and Environment settings")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        for node in ("26", "-prod5", "prod.5", " prod5", "prod5 ", "prod5\n", "prod5; echo unsafe"):
            with self.subTest(node=node):
                self.env["PHALA_NODE"] = node
                result = self.run_step("Check inputs and Environment settings")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("PHALA_NODE must be empty (auto) or a physical node name", result.stdout)

    def test_legacy_node_id_requires_explicit_operator_migration(self):
        for node in ("", "prod5"):
            with self.subTest(node=node):
                self.env.update(PHALA_NODE=node, PHALA_NODE_ID="26")
                result = self.run_step("Check inputs and Environment settings")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("PHALA_NODE_ID is no longer supported: remove it and set PHALA_NODE", result.stdout)
                self.assertFalse((self.tmp / "cli.jsonl").exists())

    def test_node_selection_and_summary(self):
        for mode, node, variant in (("provision", "prod5", "service"), ("provision", "", "service"),
                                    ("upgrade", "prod5", "service"), ("upgrade", "prod5", "product")):
            with self.subTest(mode=mode, node=node, variant=variant):
                self.env.update(MODE=mode, PHALA_NODE=node, VARIANT=variant,
                                TARGET="topup" if variant == "service" else "product")
                for path in (self.tmp / "cli.jsonl", self.tmp / "summary", self.tmp / "compose-committed"):
                    path.unlink(missing_ok=True)
                result = self.run_step("Deploy (${{ inputs.mode }}) and wait for the CVM")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                calls = [json.loads(line) for line in (self.tmp / "cli.jsonl").read_text().splitlines()]
                if mode == "provision":
                    deployment = next(args for args in calls if args[0] == "deploy")
                    self.assertNotIn("--node-id", deployment)
                    if node:
                        self.assertEqual(deployment[deployment.index("--node") + 1], node)
                    else:
                        self.assertNotIn("--node", deployment)
                    self.assertFalse(any(args[0] == "api" for args in calls))
                else:
                    self.assertFalse(any(args[0] == "deploy" for args in calls))
                    updates = [args for args in calls if args[0] == "api"]
                    self.assertEqual([args[1] for args in updates], ["/cvms/cvm-1/compose_file",
                                     "/cvms/cvm-1/compose_file/provision", "/cvms/cvm-1/compose_file"])
                    for update in updates:
                        self.assertNotIn("--node", update)
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


class ExportRollbackWorkflowTests(unittest.TestCase):
    setUp = DeployWorkflowTests.setUp
    run_step = DeployWorkflowTests.run_step
    calls = DeployWorkflowTests.calls
    set_current_image = DeployWorkflowTests.set_current_image

    def prepare_rollback(self):
        """Real renders, compose binding and policy; only the external quote verifier is stubbed."""
        deploy = self.kit / "deploy"
        for script in ("verify-attestation.sh", "attested-compose.sh", "pinned-compose.sh"):
            shutil.copy2(ROOT / "deploy" / script, deploy)
        # The renderer uses the real kit's compose/config files and the reviewed environment.
        render = deploy / "render.sh"
        render.write_text('#!/bin/sh\nexec "$REAL_RENDER" "$@"\n')
        render.chmod(0o755)
        verifier = deploy / "dstack-verifier.sh"
        verifier.write_text('''#!/usr/bin/env bash
set -euo pipefail
request=$(cat)
jq -c '["verify-quote", .quote]' <<<"$request" >>"$STUB_LOG"
quote=$(jq -er .quote <<<"$request")
if [[ "${1:-}" == --partial ]]; then echo '["partial-mode"]' >>"$STUB_LOG"
else echo '["full-mode"]' >>"$STUB_LOG"; fi
cat "$STUB_RESULTS/$quote.json"
''')
        verifier.chmod(0o755)
        self.env.update(MODE="upgrade", VARIANT="service", REAL_RENDER=str(ROOT / "deploy/render.sh"),
                        ENVIRONMENT_DIR=str(ROOT / "deploy/environments/phala-network/staging/topup"),
                        MIGRATION_PLAN=str(self.tmp / "export.json"), STUB_RESULTS=str(self.tmp),
                        CVM_ID="cvm-1", APP_ID="app-1", DOMAIN="example.invalid")
        images = {"phala-pay": "ghcr.io/phala-network/phala-pay@sha256:" + "1" * 64,
                  "postgres-walg": "ghcr.io/phala-network/postgres-walg@sha256:" + "2" * 64}
        (self.work / "release/images.json").write_text(json.dumps(images))
        pins = json.loads((ROOT / "deploy/tests/migration-pins.json").read_text())
        (self.tmp / "export.json").write_text(json.dumps({"migration_export": {
            "recipient": pins["recipient"], "object": "s3://migration-test/rollback"}}))
        for name, args in (("source", ["--export", "--migration-export", self.env["MIGRATION_PLAN"]]),
                           ("target", ["--gateway-domain", "gateway.example.net"])):
            result = subprocess.run([str(render), "--images", str(self.work / "release/images.json"),
                                     *args, self.env["ENVIRONMENT_DIR"]],
                                    env=self.env, capture_output=True, text=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            (self.tmp / f"{name}.yml").write_text(result.stdout)
        (self.work / "rollback-export-info.json").write_text('{"vm_config":"fixture-vm-config"}')
        self.write_attestation("source", self.tmp / "source.yml")
        self.write_attestation("target", self.tmp / "target.yml")
        shutil.copy2(self.tmp / "target.yml", self.env["COMPOSE"])

    def write_attestation(self, name, compose):
        app = {"name": name, "docker_compose_file": compose.read_text(),
               "pre_launch_script": (ROOT / "deploy/phala-cloud-pre-launch.sh").read_text(),
               "allowed_envs": ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "SENTRY_DSN",
                                "TOPUP_RPC_ANKR_KEY", "TOPUP_RPC_INFURA_KEY"],
               "port_policy": {"ports": [{"port": 443, "pp": True}], "restrict_mode": False}}
        app_text = json.dumps(app)
        digest = hashlib.sha256(app_text.encode()).hexdigest()
        (self.tmp / f"{name}-app.json").write_text(app_text)
        os_hash = (Path(self.env["ENVIRONMENT_DIR"]) / "os-image-hash").read_text().strip()
        attestation = {"compose_file": app_text, "app_certificates": [{"position_in_chain": 0, "quote": name}],
                       "tcb_info": {"event_log": [{"imr":3,"event": "compose-hash", "event_payload": digest},
                                                   {"imr":3,"event":"app-id","event_payload":"app-1"},
                                                   {"imr":3,"event":"os-image-hash","event_payload":os_hash}]}}
        (self.tmp / f"{name}-attestation.json").write_text(json.dumps(attestation))
        os_hash = (Path(self.env["ENVIRONMENT_DIR"]) / "os-image-hash").read_text().strip()
        verified = {"is_valid": True, "details": {"quote_verified": True, "tee_variant":"dstack-tdx", "tcb_status": "UpToDate", "app_info": {
            "app_id": "app-1", "compose_hash": digest, "os_image_hash": os_hash}}}
        (self.tmp / f"{name}.json").write_text(json.dumps(verified))
        self.env[f"STUB_{name.upper()}_HASH"] = digest
        self.env[f"STUB_{name.upper()}_ATTESTATION"] = str(self.tmp / f"{name}-attestation.json")
        if name == "source":
            self.env["STUB_SOURCE_APP"] = str(self.tmp / "source-app.json")
            (self.work / "cvm-before.json").write_text(json.dumps({
                "os": {"name": "dstack-0.6.1"}, "app_id": "app-1", "compose_hash": digest,
                "gateway": {"base_domain": "example.net"}}))

    def assert_no_mutation(self):
        self.assertFalse((self.tmp / "compose-committed").exists())
        self.assertFalse(any(call[0] in ("maintenance", "health", "api", "deploy") for call in self.calls()))
        self.assertFalse((self.tmp / "output").exists())

    def test_export_to_service_skips_maintenance_and_verifies_both_composes(self):
        self.prepare_rollback()
        result = self.run_step("Pause new mutations before upgrading")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse((self.tmp / "output").exists())  # Neither resume condition is enabled.
        self.assertIn("Routes remain paused", (self.tmp / "summary").read_text())
        for step in ("Deploy (${{ inputs.mode }}) and wait for the CVM",
                     "Read back and verify the attested compose", "Wait for the service"):
            result = self.run_step(step)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        calls = self.calls()
        self.assertFalse(any(call[0] == "maintenance" for call in calls))
        self.assertEqual([call for call in calls if call[0] == "verify-quote"],
                         [["verify-quote", "source"], ["verify-quote", "target"]])
        self.assertTrue(any(call[0:2] == ["api", "/cvms/cvm-1/compose_file/provision"] for call in calls))
        self.assertEqual(len([call for call in calls if call[0] == "health"]), 2)
        self.assertEqual(json.loads((self.work / "cvm.json").read_text())["compose_hash"],
                         self.env["STUB_TARGET_HASH"])
        self.assertIn("service policy passed", (self.work / "verification.txt").read_text())
        ingress = self.kit / "deploy/verify-ingress-evidence.sh"
        ingress.write_text('''#!/usr/bin/env bash
jq -nc '["ingress"] + $ARGS.positional' --args -- "$@" >>"$STUB_LOG"
echo 'certificate evidence verified'
''')
        ingress.chmod(0o755)
        result = self.run_step("Verify the certificate evidence")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.calls()[-1], ["ingress", "example.invalid", "app-1"])
        # TLS acceptance and both maintenance-clear steps keep their normal conditions.
        workflow = (ROOT / ".github/workflows/deploy.yml").read_text()
        for step in ("Resume mutations after health checks", "Clear maintenance after a failed upgrade"):
            block = workflow.split(f"      - name: {step}\n", 1)[1].split("        env:", 1)[0]
            self.assertIn("steps.maintenance.outputs.enabled == 'true'", block)

    def test_export_captures_service_info_and_reuses_it_after_fencing(self):
        self.prepare_rollback()
        self.env["VARIANT"] = "export"
        # Before the export upgrade the CVM must still serve the verified service compose.
        self.write_attestation("source", self.tmp / "target.yml")
        shutil.copy2(self.tmp / "target.yml", self.work / "service-domain.yml")
        result = self.run_step("Capture verified service Info before export")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(len([call for call in self.calls() if call[0] == "info"]), 1)
        # The export quote has the export compose; there is no ingress left to fetch Info.
        self.write_attestation("target", self.tmp / "source.yml")
        shutil.copy2(self.tmp / "source.yml", self.env["COMPOSE"])
        (self.work / "cvm.json").write_text(json.dumps({"compose_hash": self.env["STUB_TARGET_HASH"]}))
        (self.tmp / "compose-committed").touch()
        self.env["STUB_INFO_STATUS"] = "42"
        result = self.run_step("Read back and verify the attested compose")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(len([call for call in self.calls() if call[0] == "info"]), 1)

    def test_pre_export_info_failure_stops_before_mutation(self):
        self.prepare_rollback()
        self.env.update(VARIANT="export", STUB_INFO_STATUS="42")
        result = self.run_step("Capture verified service Info before export")
        self.assertEqual(result.returncode, 42, result.stdout + result.stderr)
        self.assert_no_mutation()

    def test_unsealed_provision_partially_verifies_before_sealing(self):
        self.prepare_rollback()
        self.env.update(MODE="provision", STUB_INFO_STATUS="42",
                        STUB_SOURCE_ATTESTATION=self.env["STUB_TARGET_ATTESTATION"])
        partial = json.loads((self.tmp / "target.json").read_text())
        partial["is_valid"] = False
        partial["details"]["app_info"] = None
        (self.tmp / "target.json").write_text(json.dumps(partial))
        (self.work / "cvm.json").write_text(json.dumps({"compose_hash": self.env["STUB_TARGET_HASH"]}))
        result = self.run_step("Read back and verify the attested compose")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue((self.work / "acceptance-pending.txt").exists())
        self.assertFalse((self.work / "verification.txt").exists())
        self.assertIn("partial: quote + event log + compose/app, OS/vm_config not verified",
                      (self.work / "partial-verification.txt").read_text())
        self.assertIn(["partial-mode"], self.calls())
        self.assertFalse(any(call[0] == "info" for call in self.calls()))

    def test_partial_only_result_cannot_pass_upgrade_acceptance(self):
        self.prepare_rollback()
        self.env["STUB_SOURCE_ATTESTATION"] = self.env["STUB_TARGET_ATTESTATION"]
        (self.work / "cvm.json").write_text(json.dumps({"compose_hash": self.env["STUB_TARGET_HASH"]}))
        partial = json.loads((self.tmp / "target.json").read_text())
        partial["is_valid"] = False
        partial["details"]["app_info"] = None
        (self.tmp / "target.json").write_text(json.dumps(partial))
        result = self.run_step("Read back and verify the attested compose")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(["full-mode"], self.calls())
        self.assertNotIn(["partial-mode"], self.calls())
        self.assertFalse((self.work / "acceptance-pending.txt").exists())

    def test_rollback_downloads_info_and_checks_cvm_identity(self):
        import zipfile
        self.prepare_rollback()
        self.env.update(SOURCE_INFO_ARTIFACT="123", GITHUB_REPOSITORY="owner/repo")
        cvm = json.loads((self.work / "cvm-before.json").read_text())
        cvm["vm_uuid"] = "cvm-1"
        (self.work / "cvm-before.json").write_text(json.dumps(cvm))
        archive = self.tmp / "artifact.zip"
        gh = self.tmp / "bin/gh"
        gh.write_text("#!/bin/sh\ncase \"$2\" in */zip) cat \"$RUNNER_TEMP/artifact.zip\" ;; *) printf '%s' '{\"expired\":false,\"name\":\"production-topup-deploy-99-1\"}' ;; esac\n")
        gh.chmod(0o755)
        for uuid, expected in (("cvm-1", 0), ("other", 1)):
            cvm["vm_uuid"] = uuid
            with zipfile.ZipFile(archive, "w") as output:
                output.writestr("info.json", '{"vm_config":"fixture-vm-config"}')
                output.writestr("cvm.json", json.dumps(cvm))
            result = self.run_step("Download the export guest Info for rollback")
            self.assertEqual(result.returncode, expected, result.stdout + result.stderr)

    def test_rollback_rejects_an_unexpected_target_compose_after_deployment(self):
        self.prepare_rollback()
        self.write_attestation("target", self.tmp / "source.yml")
        for step in ("Pause new mutations before upgrading", "Deploy (${{ inputs.mode }}) and wait for the CVM"):
            result = self.run_step(step)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        result = self.run_step("Read back and verify the attested compose")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("attested docker_compose_file differs", result.stdout)
        self.assertFalse((self.tmp / "output").exists())
        self.assertFalse(any(call[0] == "maintenance" for call in self.calls()))

    def test_other_upgrades_still_require_a_successful_pause(self):
        for mode, variant in (("upgrade", "service"), ("upgrade", "export"), ("commit", "service")):
            with self.subTest(mode=mode, variant=variant):
                (self.tmp / "cli.jsonl").unlink(missing_ok=True)
                self.env.update(MODE=mode, VARIANT=variant, STUB_PAUSE_STATUS="42")
                # Migration plans for export/import never select the service rollback path.
                self.env["MIGRATION_PLAN"] = "" if variant == "service" else "migration.json"
                result = self.run_step("Pause new mutations before upgrading")
                self.assertEqual(result.returncode, 42, result.stdout + result.stderr)
                self.assertEqual([call[0] for call in self.calls()], ["maintenance"])
                self.assertFalse((self.tmp / "output").exists())

    def test_import_render_does_not_extract_a_domain_from_an_ingressless_compose(self):
        render = self.kit / "deploy/render.sh"
        render.write_text('#!/bin/sh\nprintf "services: {}\\n"\n')
        render.chmod(0o755)
        self.env.update(VARIANT="migration-import", MIGRATION_PLAN="migration.json", GATEWAY="gateway.example.invalid")
        result = self.run_step("Render the environment with the release's kit and images")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("PUBLIC_URL=\n", (self.tmp / "env").read_text())
        self.assertIn("DOMAIN=\n", (self.tmp / "env").read_text())
        self.assertIn("no API or ingress", (self.tmp / "summary").read_text())

    def test_service_source_cannot_claim_to_be_the_reviewed_export(self):
        self.prepare_rollback()
        self.write_attestation("source", self.tmp / "target.yml")
        result = self.run_step("Pause new mutations before upgrading")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("attested docker_compose_file differs", result.stdout)
        self.assert_no_mutation()

    def test_export_render_mismatch_fails_before_mutation(self):
        self.prepare_rollback()
        plan = json.loads((self.tmp / "export.json").read_text())
        plan["migration_export"]["object"] = "s3://migration-test/different-run"
        (self.tmp / "export.json").write_text(json.dumps(plan))
        result = self.run_step("Pause new mutations before upgrading")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("attested docker_compose_file differs", result.stdout)
        self.assert_no_mutation()

    def test_unverified_source_quote_identity_or_os_fails_before_mutation(self):
        self.prepare_rollback()
        verified_path = self.tmp / "source.json"
        original = verified_path.read_text()
        for field, value in (("is_valid", False), ("tcb_status", "OutOfDate"),
                             ("app_id", "wrong-app"), ("os_image_hash", "0" * 64),
                             ("compose_hash", "0" * 64)):
            with self.subTest(field=field):
                verified = json.loads(original)
                if field == "is_valid":
                    verified[field] = value
                elif field == "tcb_status":
                    verified["details"][field] = value
                else:
                    verified["details"]["app_info"][field] = value
                verified_path.write_text(json.dumps(verified))
                result = self.run_step("Pause new mutations before upgrading")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("verified attestation does not match", result.stdout)
                self.assert_no_mutation()

    def test_os_upgrade_and_info_failure_cannot_bypass_maintenance(self):
        self.prepare_rollback()
        self.set_current_image("dstack-0.5.12")
        result = self.run_step("Pause new mutations before upgrading")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Export rollback cannot change the OS", result.stdout)
        self.assertFalse((self.tmp / "cli.jsonl").exists())
        self.write_attestation("source", self.tmp / "source.yml")
        (self.work / "rollback-export-info.json").unlink()
        result = self.run_step("Pause new mutations before upgrading")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_no_mutation()

    def test_rollback_input_guards(self):
        self.env["SOURCE_INFO_ARTIFACT"] = "123"
        self.env.update(REQUESTED_VARIANT="service", TOPUP_CVM_ID="cvm-1", MODE="upgrade",
                        MIGRATION_PLAN="migration/export.json")
        result = self.run_step("Check inputs and Environment settings")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        for mode in ("provision", "prepare", "commit"):
            self.env["MODE"] = mode
            self.assertNotEqual(self.run_step("Check inputs and Environment settings").returncode, 0)
        self.env["MODE"] = "upgrade"
        for plan in ("../export.json", "/export.json", "export.json\n", "$(echo unsafe)"):
            self.env["MIGRATION_PLAN"] = plan
            self.assertNotEqual(self.run_step("Check inputs and Environment settings").returncode, 0)
        self.env.update(TARGET="product", DEPLOY_ENVIRONMENT="staging", MIGRATION_PLAN="export.json")
        self.assertNotEqual(self.run_step("Check inputs and Environment settings").returncode, 0)


if __name__ == "__main__":
    unittest.main()
