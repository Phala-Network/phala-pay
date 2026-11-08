#!/usr/bin/env python3
"""Behavioral policy and reusable workflow input guards for both KMS kinds."""
import json
from pathlib import Path
import subprocess
import unittest

import test_deploy_workflow

ROOT = Path(__file__).resolve().parents[2]


class KmsPolicyTests(unittest.TestCase):
    def test_port_policy_rejects_array_variants_instead_of_matching_subarrays(self):
        for variant in [['service'], ['service', 'restore-check'], []]:
            with self.subTest(variant=variant):
                result = subprocess.run(['jq', '-n', '-L', str(ROOT / 'deploy'),
                    '--argjson', 'variant', json.dumps(variant),
                    'include "compose-policy"; port_policy($variant)'],
                    capture_output=True, text=True, check=False)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('unknown variant', result.stderr)

    def violations(self, kind, app, identity='3059'):
        result = subprocess.run(['jq', '-c', '-L', str(ROOT / 'deploy'), '--arg', 'kind', kind,
                                 '--arg', 'identity', identity,
                                 'include "compose-policy"; kms_violations($kind; .; $identity)'],
                                input=json.dumps(app), capture_output=True, text=True, check=True)
        return json.loads(result.stdout)

    def test_cloud_path_is_compatible_and_base_requires_all_provider_pins(self):
        self.assertEqual(self.violations('phala', {}), [])
        app = {'kms_enabled': True, 'key_provider': 'kms', 'key_provider_id': '3059'}
        self.assertEqual(self.violations('base', app), [])
        for field, bad in [('kms_enabled', False), ('key_provider', 'local'), ('key_provider_id', '3058')]:
            with self.subTest(field=field):
                self.assertTrue(self.violations('base', {**app, field: bad}))
                missing = dict(app)
                del missing[field]
                self.assertTrue(self.violations('base', missing))
        self.assertTrue(self.violations('ethereum', app))
        self.assertTrue(self.violations('base', app, ''))

    def test_export_alone_can_retain_unused_service_secret_names(self):
        compose = {'services': {'export': {'environment': {
            'AWS_ACCESS_KEY_ID': '${AWS_ACCESS_KEY_ID:-}',
            'AWS_SECRET_ACCESS_KEY': '${AWS_SECRET_ACCESS_KEY:-}'}}}}
        retained = ['AWS_ACCESS_KEY_ID', 'AWS_SECRET_ACCESS_KEY', 'SENTRY_DSN', 'TOPUP_RPC_ANKR_KEY']
        def check(variant, names):
            result = subprocess.run(['jq', '-c', '-L', str(ROOT / 'deploy'), '--arg', 'variant', variant,
                '--argjson', 'names', json.dumps(names),
                'include "compose-policy"; allowed_envs_violations($variant; $names)'],
                input=json.dumps(compose), capture_output=True, text=True, check=True)
            return json.loads(result.stdout)
        self.assertEqual(check('export', retained), [])
        self.assertTrue(check('export', retained + ['DSTACK_AUTHORIZED_KEYS']))
        self.assertTrue(check('export', retained + ['DATABASE_URL']))
        for variant in ['service', 'restore-check', 'template', 'product', 'migration-import']:
            self.assertTrue(check(variant, retained))

    def test_environment_defaults_do_not_switch_existing_apps(self):
        prepared = ROOT / 'deploy/environments/phala-network/staging/topup-okms'
        for file in (ROOT / 'deploy/environments').rglob('kms'):
            if file.parent == prepared:
                self.assertEqual(file.read_text().strip(), 'base')
                continue
            self.assertEqual(file.read_text().strip(), 'phala')
            self.assertEqual((file.parent / 'dstack-app').read_text(), '')


class OnchainWorkflowInputs(unittest.TestCase):
    setUp = test_deploy_workflow.DeployWorkflowTests.setUp
    run_step = test_deploy_workflow.DeployWorkflowTests.run_step
    calls = test_deploy_workflow.DeployWorkflowTests.calls
    set_current_image = test_deploy_workflow.DeployWorkflowTests.set_current_image

    def test_commit_validates_transaction_hash_before_any_cloud_call(self):
        self.env.update(MODE='commit', TRANSACTION_HASH='already-registered')
        self.assertEqual(self.run_step('Check inputs and Environment settings').returncode, 0)
        self.env['TRANSACTION_HASH'] = '0x' + 'a' * 64
        self.assertEqual(self.run_step('Check inputs and Environment settings').returncode, 0)
        for value in ['', 'bad', '0x123', '0x' + 'a' * 64 + '\n', '$(echo unsafe)']:
            self.env['TRANSACTION_HASH'] = value
            result = self.run_step('Check inputs and Environment settings')
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('commit requires transaction_hash', result.stdout)

    def test_migration_variant_input_guards(self):
        self.env.update(MODE='upgrade', TOPUP_CVM_ID='cvm-1', REQUESTED_VARIANT='export', MIGRATION_PLAN='deploy/export.json')
        self.assertEqual(self.run_step('Check inputs and Environment settings').returncode, 0)
        self.env['TOPUP_CVM_ID'] = ''
        self.assertNotEqual(self.run_step('Check inputs and Environment settings').returncode, 0)
        self.env.update(TOPUP_CVM_ID='cvm-1', REQUESTED_VARIANT='migration-import', MODE='prepare')
        self.assertEqual(self.run_step('Check inputs and Environment settings').returncode, 0)
        self.env['MIGRATION_PLAN'] = '../outside.json'
        self.assertNotEqual(self.run_step('Check inputs and Environment settings').returncode, 0)

    def test_export_upgrade_waits_for_attestation_hash_without_service_health(self):
        self.env.update(MODE='upgrade', VARIANT='export')
        # PR 2 contributes this variant's service-style port policy. Keep PR 1's workflow
        # behavior under test without copying the sibling implementation into this branch.
        with (self.kit / 'deploy/compose-policy.jq').open('a') as stream:
            stream.write('\ndef port_policy($variant): {ports:[{port:443,pp:true}],restrict_mode:false};\n')
        result = self.run_step('Deploy (${{ inputs.mode }}) and wait for the CVM')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(any(call[0] == 'health' for call in self.calls()))
        self.assertTrue(any(call[0:2] == ['api', '/cvms/cvm-1/compose_file/provision'] for call in self.calls()))

    def test_prepare_does_not_require_maintenance_key_or_transaction(self):
        self.env.update(MODE='prepare', TRANSACTION_HASH='')
        self.env.pop('TOPUP_MAINTENANCE_PRIVATE_KEY_PEM', None)
        self.assertEqual(self.run_step('Check inputs and Environment settings').returncode, 0)


if __name__ == '__main__':
    unittest.main()
