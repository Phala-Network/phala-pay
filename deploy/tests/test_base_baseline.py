#!/usr/bin/env python3
"""Unreviewed Cloud app-compose metadata cannot reach a Safe batch or pass attestation."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
APP = '0x' + '11' * 20
SAFE = '0x8AB55148944F837040bbB887f8A5005f70FF7700'
BASELINE = {
    'docker_compose_file': 'services: {}\n', 'pre_launch_script': 'true\n',
    'allowed_envs': ['AWS_ACCESS_KEY_ID'], 'kms_enabled': True, 'key_provider': 'kms',
    'key_provider_id': '3059', 'local_key_provider_enabled': False, 'no_instance_id': False,
    'public_logs': False, 'public_sysinfo': False, 'public_tcbinfo': True, 'secure_time': True,
    'gateway_enabled': True, 'tproxy_enabled': True, 'storage_fs': 'ext4', 'runner': 'docker-compose',
    'manifest_version': 3, 'features': ['kms', 'tproxy-net'],
    'port_policy': {'ports': [{'port': 443, 'pp': True}], 'restrict_mode': False},
}
MUTATIONS = {
    'root init script': {'init_script': ['unreviewed root command']},
    'extra key': {'unknown_future_option': True},
    'added env name': {'allowed_envs': BASELINE['allowed_envs'] + ['BASH_ENV']},
    'flipped flag': {'public_sysinfo': True},
}


class BaseBaselineTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='base-baseline-tests-')
        self.addCleanup(self.directory.cleanup)
        self.tmp = Path(self.directory.name)
        self.deploy = self.tmp / 'kit/deploy'
        self.deploy.mkdir(parents=True)
        for name in ['phala-cvm.sh', 'phala-onchain.sh', 'app-compose-baseline.sh', 'deadline.sh',
                     'compose-policy.jq', 'onchain-kms.json', 'verify-attestation.sh']:
            shutil.copy2(ROOT / 'deploy' / name, self.deploy)
        self.environment = self.tmp / 'environment'
        self.environment.mkdir()
        for name, value in [('kms', 'base\n'), ('dstack-app', APP + '\n'), ('dstack-owner', SAFE + '\n')]:
            (self.environment / name).write_text(value)
        (self.environment / 'app-compose.json').write_text(json.dumps(BASELINE))
        self.current = self.tmp / 'current.json'
        self.cvm = self.tmp / 'cvm.json'
        self.cvm.write_text(json.dumps({'app_id': APP, 'kms_type': 'BASE', 'kms_info': {
            'chain_id': 8453, 'dstack_kms_address': '0x2f83172A49584C017F2B256F0FB2Dca14126Ba9C'}}))
        phala = self.deploy / 'phala'
        phala.write_text('''#!/bin/sh
case "$1 $2" in
  "cvms get") cat "$FIXTURE_CVM" ;;
  "api /cvms/cvm-test/compose_file") cat "$FIXTURE_APP" ;;
  *) echo 'unexpected Cloud mutation' >&2; exit 99 ;;
esac
''')
        phala.chmod(0o755)
        self.compose = self.tmp / 'compose.yml'
        self.compose.write_text(BASELINE['docker_compose_file'])
        self.pre = self.tmp / 'pre.sh'
        self.pre.write_text(BASELINE['pre_launch_script'])
        self.env = {**os.environ, 'FIXTURE_APP': str(self.current), 'FIXTURE_CVM': str(self.cvm)}

    def run_script(self, *args):
        return subprocess.run(list(map(str, args)), env=self.env, capture_output=True, text=True, timeout=10)

    def test_each_injected_field_refuses_provision_and_upgrade_before_batch_or_cloud_mutation(self):
        for name, mutation in MUTATIONS.items():
            with self.subTest(name=name):
                altered = {**BASELINE, **mutation}
                self.current.write_text(json.dumps(altered))
                provision = self.tmp / 'provision.json'
                provision.write_text(json.dumps({'kms': 'BASE', 'node_id': 26, 'compose_file': altered}))
                for mode in ['provision-prepare', 'compose-prepare']:
                    output = self.tmp / (mode + '.json')
                    args = [provision, output, self.environment] if mode == 'provision-prepare' else [
                        'cvm-test', self.compose, self.pre, json.dumps(BASELINE['port_policy']), output, self.environment]
                    result = self.run_script(self.deploy / 'phala-cvm.sh', mode, *args)
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertIn('Base app-compose', result.stderr)
                    self.assertNotIn('unexpected Cloud mutation', result.stderr)
                    self.assertFalse(Path(str(output) + '.batch.json').exists())

    def test_provision_commit_requires_the_committed_app_before_any_chain_or_cloud_call(self):
        (self.environment / 'dstack-app').write_text('')
        result = self.run_script(self.deploy / 'phala-cvm.sh', 'provision-commit',
                                 '0x' + 'ab' * 32, self.tmp / 'prepare.json', self.environment)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("commit the receipt's dstack-app", result.stderr)

    def test_commit_response_does_not_print_arbitrary_body_or_stderr(self):
        prepared = self.tmp / 'prepare.json'
        prepared.write_text(json.dumps({'compose_hash': 'ab' * 32, 'device_id': 'cd' * 32,
            'token': 'private-test-token', 'kms_info': {'chain_id': 8453,
            'kms_contract_address': '0x2f83172A49584C017F2B256F0FB2Dca14126Ba9C'}}))
        Path(str(prepared) + '.request.json').write_text(json.dumps(BASELINE))
        phala = self.deploy / 'phala'
        phala.write_text("""#!/bin/sh
printf '%s\\n' '{"status":"in_progress","correlation_id":"fixture-id","token":"private-test-token"}'
printf 'private-test-token\\n' >&2
""")
        phala.chmod(0o755)
        verify = self.deploy / 'verify-onchain-app.sh'
        verify.write_text('#!/bin/sh\nexit 0\n')
        verify.chmod(0o755)
        bin_dir = self.tmp / 'bin'
        bin_dir.mkdir()
        node = bin_dir / 'node'
        node.write_text('#!/bin/sh\nprintf "confirmed\\n"\n')
        node.chmod(0o755)
        self.env['PATH'] = str(bin_dir) + ':' + self.env['PATH']
        result = self.run_script(self.deploy / 'phala-cvm.sh', 'compose-commit', 'cvm-test',
                                 'ab' * 32, 'already-registered', prepared, self.environment)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn('private-test-token', result.stdout + result.stderr)
        self.assertEqual(json.loads(result.stdout), {'status': 'in_progress', 'correlation_id': 'fixture-id'})

    def test_policy_accepts_exact_baseline_but_forbids_init_commands_even_in_a_baseline(self):
        for value, passes in [(BASELINE, True), ({**BASELINE, 'init_script': []}, True),
                              ({**BASELINE, 'init_script': ['command']}, False)]:
            result = subprocess.run(['jq', '-e', '-L', str(self.deploy), 'include "compose-policy"; '
                'base_app_violations(.; .) | length == 0'], input=json.dumps(value),
                capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode == 0, passes, result.stderr)

    def test_each_injected_field_is_rejected_after_quote_replay_at_attestation(self):
        scripts = {
            'dstack-verifier.sh': '''#!/bin/sh
cat >/dev/null
cat "$FIXTURE_RESULT"
''',
            'attested-compose.sh': '''#!/bin/sh
cp "$FIXTURE_APP" "$3/app-compose.json"
printf 'services: {}\\n' >"$3/docker-compose.yml"
printf 'fixture-compose-hash\\n'
''',
        }
        for name, script in scripts.items():
            path = self.deploy / name
            path.write_text(script)
            path.chmod(0o755)
        bin_dir = self.tmp / 'bin'
        bin_dir.mkdir()
        node = bin_dir / 'node'
        node.write_text('#!/bin/sh\nprintf "3059\\n"\n')
        node.chmod(0o755)
        self.env['PATH'] = str(bin_dir) + ':' + self.env['PATH']
        result_file = self.tmp / 'verified.json'
        result_file.write_text(json.dumps({'is_valid': True, 'details': {'tcb_status': 'UpToDate', 'app_info': {
            'app_id': APP[2:], 'compose_hash': 'fixture-compose-hash', 'os_image_hash': 'ab' * 32}}}))
        self.env['FIXTURE_RESULT'] = str(result_file)
        attestation = self.tmp / 'attestation.json'
        attestation.write_text(json.dumps({'app_certificates': [{'position_in_chain': 0, 'quote': 'fixture'}],
                                          'tcb_info': {'event_log': []}}))
        info = self.tmp / 'info.json'
        info.write_text(json.dumps({'vm_config': '{}'}))
        for name, mutation in MUTATIONS.items():
            with self.subTest(name=name):
                self.current.write_text(json.dumps({**BASELINE, **mutation}))
                result = self.run_script(self.deploy / 'verify-attestation.sh', attestation, info, APP,
                                         self.compose, 'service', 'ab' * 32, self.environment)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn('Base app-compose', result.stderr)


if __name__ == '__main__':
    unittest.main()
