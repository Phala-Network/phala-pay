#!/usr/bin/env python3
"""Exercise the real compose-update helper against a CLI stub, never a CVM."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
POLICY = {"ports": [{"port": 443, "pp": False}], "restrict_mode": False}


class ComposeUpdateTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="compose-update-")
        self.addCleanup(self.directory.cleanup)
        self.tmp = Path(self.directory.name)
        for name in ("phala-cvm.sh", "deadline.sh"):
            shutil.copy2(ROOT / "deploy" / name, self.tmp)
        cli = self.tmp / "phala"
        cli.write_text('''#!/usr/bin/env python3
import json, os, sys, time
from pathlib import Path
args = sys.argv[1:]
method = args[args.index("-X") + 1] if "-X" in args else "GET"
operation = "CVM" if args[:2] == ["cvms", "get"] else method
call = {"args": args, "method": method}
if "--input" in args:
    call["body"] = json.loads(Path(args[args.index("--input") + 1]).read_text())
with open(os.environ["STUB_LOG"], "a") as log:
    log.write(json.dumps(call) + "\\n")
if operation == os.environ.get("STUB_HANG"):
    time.sleep(10)
if operation == os.environ.get("STUB_FAIL"):
    print('{"error":"operation failed","request_id":"req-compose-test"}')
    sys.exit(42)
if operation == "CVM":
    print(os.environ.get("STUB_CVM", json.dumps({"compose_hash": "cd" * 32})))
elif method == "GET":
    print(os.environ["STUB_CURRENT"])
elif method == "POST":
    print(os.environ.get("STUB_PROVISION", json.dumps({"compose_hash": "ab" * 32, "app_id": "app-1"})))
''')
        cli.chmod(0o755)
        self.current = {"manifest_version": 2, "docker_compose_file": "old compose\n",
                        "pre_launch_script": "old script\n", "allowed_envs": ["AWS_SECRET_ACCESS_KEY"],
                        "key_provider": "kms", "key_provider_id": "owner-kms", "storage_fs": "ext4",
                        "public_tcbinfo": True, "public_logs": True, "public_sysinfo": True,
                        "port_policy": {"ports": [{"port": 443, "pp": True}], "restrict_mode": False}}
        (self.tmp / "compose.yml").write_text('services:\n  app:\n    command: ["literal $value", "line\\nvalue"]\n')
        (self.tmp / "pre-launch.sh").write_text('#!/bin/sh\nprintf \'%s\\n\' "$DSTACK_APP_ID"\n')
        self.env = {**os.environ, "STUB_LOG": str(self.tmp / "calls"),
                    "STUB_CURRENT": json.dumps(self.current)}

    def run_update(self, policy=POLICY, cvm="cvm-1"):
        return subprocess.run([str(self.tmp / "phala-cvm.sh"), "compose-update", cvm,
                               str(self.tmp / "compose.yml"), str(self.tmp / "pre-launch.sh"),
                               json.dumps(policy), str(self.tmp / "out.json")],
                              env=self.env, text=True, capture_output=True, timeout=10)

    def calls(self):
        path = self.tmp / "calls"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def test_preserves_settings_and_sealed_env_in_one_app_compose(self):
        result = self.run_update()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        self.assertEqual([call["method"] for call in calls], ["GET", "GET", "POST", "PATCH"])
        self.assertEqual(calls[0]["args"], ["cvms", "get", "cvm-1", "--json"])
        self.assertEqual([call["args"][1] for call in calls[1:]], ["/cvms/cvm-1/compose_file",
                          "/cvms/cvm-1/compose_file/provision", "/cvms/cvm-1/compose_file"])
        expected = {**self.current, "docker_compose_file": (self.tmp / "compose.yml").read_text(),
                    "pre_launch_script": (self.tmp / "pre-launch.sh").read_text(),
                    "port_policy": POLICY, "public_logs": False, "public_sysinfo": False}
        self.assertEqual(calls[2]["body"], expected)
        self.assertEqual(calls[3]["body"], {"compose_hash": "ab" * 32})
        self.assertEqual(json.loads((self.tmp / "out.json").read_text())["app_id"], "app-1")
        self.assertEqual(result.stdout, "")

    def test_each_api_error_stops_later_operations_and_keeps_diagnostics(self):
        for index, method in enumerate(("CVM", "GET", "POST", "PATCH"), 1):
            with self.subTest(method=method):
                (self.tmp / "calls").unlink(missing_ok=True)
                self.env["STUB_FAIL"] = method
                result = self.run_update()
                self.assertEqual(result.returncode, 42, result.stderr)
                self.assertIn("req-compose-test", result.stderr)
                self.assertEqual(len(self.calls()), index)

    def test_invalid_get_or_provision_response_prevents_commit(self):
        self.env["STUB_CURRENT"] = '{"error":"GET failed without nonzero status"}'
        result = self.run_update()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(self.calls()), 2)
        self.assertIn("GET failed", result.stderr)
        self.env["STUB_CURRENT"] = json.dumps(self.current)
        for response in ("{}", '{"compose_hash":"not-a-hash"}', '{"compose_hash":null}'):
            with self.subTest(response=response):
                (self.tmp / "calls").unlink()
                self.env["STUB_PROVISION"] = response
                result = self.run_update()
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(len(self.calls()), 3)
                self.assertIn("no valid compose hash", result.stderr)

    def test_current_hash_skips_commit_and_preserves_provision_response(self):
        for current, target, registered in (("ab" * 32, "ab" * 32, True),
                                            ("0x" + "AB" * 32, "ab" * 32, False),
                                            ("ab" * 32, "0x" + "AB" * 32, True)):
            with self.subTest(current=current, target=target, registered=registered):
                (self.tmp / "calls").unlink(missing_ok=True)
                self.env["STUB_CVM"] = json.dumps({"compose_hash": current})
                provision = {"compose_hash": target, "compose_hash_registered": registered, "app_id": "app-1"}
                self.env["STUB_PROVISION"] = json.dumps(provision)
                result = self.run_update()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual([call["method"] for call in self.calls()], ["GET", "GET", "POST"])
                self.assertIn("unchanged; nothing to commit", result.stderr)
                self.assertEqual(json.loads((self.tmp / "out.json").read_text()), provision)
                self.assertEqual(result.stdout, "")

    def test_registered_differing_hash_still_commits_and_patch_errors_fail(self):
        self.env["STUB_PROVISION"] = json.dumps({"compose_hash": "ab" * 32, "compose_hash_registered": True})
        for status in (0, 42):
            with self.subTest(status=status):
                (self.tmp / "calls").unlink(missing_ok=True)
                self.env["STUB_FAIL"] = "PATCH" if status else ""
                result = self.run_update()
                self.assertEqual(result.returncode, status, result.stderr)
                self.assertEqual([call["method"] for call in self.calls()], ["GET", "GET", "POST", "PATCH"])
                self.assertEqual(self.calls()[-1]["body"], {"compose_hash": "ab" * 32})
                self.assertNotIn("unchanged; nothing to commit", result.stderr)
                if status:
                    self.assertIn("req-compose-test", result.stderr)

    def test_missing_or_invalid_current_hash_stops_before_provision(self):
        for response in ({}, {"compose_hash": None}, {"compose_hash": "not-a-hash"}, {"compose_hash": 123}):
            with self.subTest(response=response):
                (self.tmp / "calls").unlink(missing_ok=True)
                self.env["STUB_CVM"] = json.dumps(response)
                result = self.run_update()
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual([call["args"] for call in self.calls()], [["cvms", "get", "cvm-1", "--json"]])
                self.assertIn("no valid current compose hash", result.stderr)

    def test_invalid_policy_or_cvm_never_calls_api(self):
        for policy in ({}, {**POLICY, "restrict_mode": True}, {**POLICY, "ports": [{"port": 443, "pp": "false"}]},
                       {**POLICY, "ports": [{"port": 0, "pp": False}]},
                       {**POLICY, "ports": POLICY["ports"] * 2}):
            with self.subTest(policy=policy):
                self.assertEqual(self.run_update(policy).returncode, 64)
                self.assertEqual(self.calls(), [])
        self.assertEqual(self.run_update(cvm="../other-cvm").returncode, 64)
        self.assertEqual(self.calls(), [])

    def test_stage_deadline_bounds_api_calls(self):
        self.env.update(COMPOSE_UPDATE_SECONDS="1", STUB_HANG="POST")
        result = self.run_update()
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertEqual([call["method"] for call in self.calls()], ["GET", "GET", "POST"])
        self.assertIn("stage=compose-update", result.stderr)


if __name__ == "__main__":
    unittest.main()
