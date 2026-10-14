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
        self.directory = tempfile.TemporaryDirectory(prefix=".deploy-workflow-test-", dir=ROOT)
        self.addCleanup(self.directory.cleanup)
        self.tmp = Path(self.directory.name)
        self.kit = self.tmp / "kit"
        deploy = self.kit / "deploy"
        deploy.mkdir(parents=True)
        for script in ("phala-cvm.sh", "deadline.sh"):
            shutil.copy2(ROOT / "deploy" / script, deploy)
        (deploy / "upgrade-monitor.py").write_text("# No network sampling in this workflow test.\n")
        cli = deploy / "phala"
        cli.write_text('''#!/usr/bin/env bash
set -euo pipefail
jq -nc '$ARGS.positional' --args -- "$@" >>"$STUB_LOG"
case "$1" in
    deploy) echo '{"success":true,"vm_uuid":"cvm-1","app_id":"app-1"}' ;;
    cvms) echo '{"status":"running","in_progress":false,"compose_hash":"ab12","node_info":{"id":26,"name":"prod5","region":"US-WEST-1"}}' ;;
    *) exit 64 ;;
esac
''')
        cli.chmod(0o755)
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir()
        curl = bin_dir / "curl"
        curl.write_text("#!/bin/sh\nexit 0\n")
        curl.chmod(0o755)
        self.work = self.tmp / "work"
        (self.work / "release").mkdir(parents=True)
        (self.work / "release/images.json").write_text('{"phala-pay":"test-image"}')
        self.env = {**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}",
                    "KIT": str(self.kit), "WORK": str(self.work), "RUNNER_TEMP": str(self.tmp),
                    "STUB_LOG": str(self.tmp / "cli.jsonl"), "GITHUB_ENV": str(self.tmp / "env"),
                    "GITHUB_STEP_SUMMARY": str(self.tmp / "summary"),
                    "PHALA_CLOUD_API_KEY": "test-key", "PHALA_WORKSPACE": "test-workspace",
                    "PHALA_NODE_ID": "", "TOPUP_CVM_ID": "", "STAGING_PRODUCT_CVM_ID": "",
                    "VERSION": "v0.10.4", "DEPLOY_ENVIRONMENT": "production", "TARGET": "topup",
                    "MODE": "provision", "ENVIRONMENT_DIR": "production/topup",
                    "CVM_NAME": "test-cvm", "CVM_ID_VARIABLE": "TOPUP_CVM_ID",
                    "EXISTING_CVM_ID": "cvm-1", "COMPOSE": str(self.tmp / "compose.yml"),
                    "ENV_FILE": str(self.tmp / "env-file"), "INSTANCE_TYPE": "tdx.medium",
                    "DSTACK_OS_IMAGE": "dstack-0.5.9", "IMAGES": "phala-pay",
                    "PUBLIC_URL": "https://example.invalid", "GITHUB_RUN_ID": "1",
                    "GITHUB_SHA": "test-sha", "GITHUB_SERVER_URL": "https://github.com",
                    "RELEASE_REPOSITORY": "Phala-Network/phala-pay"}

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
                summary = (self.tmp / "summary").read_text()
                requested = f"`{node or 'auto'}`" if mode == "provision" else "unchanged (upgrade)"
                self.assertIn(f"| Requested node | {requested} |", summary)
                self.assertIn("| CVM node | 26 (prod5, US-WEST-1) |", summary)


if __name__ == "__main__":
    unittest.main()
