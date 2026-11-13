#!/usr/bin/env python3
"""Exercise Base prepare/commit through the real workflow against local Cloud/RPC boundaries."""
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import re
import shutil
import threading
import unittest

import test_base_baseline
import test_deploy_workflow

ROOT = Path(__file__).resolve().parents[2]
APP = test_base_baseline.APP
SAFE = test_base_baseline.SAFE
DEVICE = 'cd' * 32


def compose_hash(compose):
    return hashlib.sha256(json.dumps(compose, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()).hexdigest()


class OnchainUpgradeTests(unittest.TestCase):
    run_step = test_deploy_workflow.DeployWorkflowTests.run_step
    calls = test_deploy_workflow.DeployWorkflowTests.calls
    set_current_image = test_deploy_workflow.DeployWorkflowTests.set_current_image

    def setUp(self):
        test_deploy_workflow.DeployWorkflowTests.setUp(self)
        deploy = self.kit / 'deploy'
        for name in ['phala-onchain.sh', 'app-compose-baseline.sh', 'onchain-kms.json', 'verify-onchain-app.sh']:
            shutil.copy2(ROOT / 'deploy' / name, deploy)
        tools = deploy / 'tools'
        tools.mkdir()
        shutil.copy2(ROOT / 'deploy/tools/onchain-kms.mjs', tools)
        (tools / 'node_modules').symlink_to(ROOT / 'deploy/tools/node_modules', target_is_directory=True)
        self.pin = json.loads((deploy / 'onchain-kms.json').read_text())
        self.environment = self.tmp / 'environment'
        self.environment.mkdir()
        for name, value in [('kms', 'base'), ('dstack-app', APP), ('dstack-owner', SAFE), ('os-image-hash', 'ab' * 32)]:
            (self.environment / name).write_text(value + '\n')
        self.baseline = test_base_baseline.BASELINE
        (self.environment / 'app-compose.json').write_text(json.dumps(self.baseline))
        self.request = {**self.baseline, 'pre_launch_script': (deploy / 'phala-cloud-pre-launch.sh').read_text()}
        self.hash = compose_hash(self.request)
        self.stored = self.tmp / 'stored.json'
        self.cvm = self.tmp / 'cvm.json'
        self.stored.write_text(json.dumps(self.baseline))
        self.cvm.write_text(json.dumps({'app_id': APP, 'kms_type': 'base', 'kms_info': {'chain_id': 8453,
            'dstack_kms_address': self.pin['kms_auth']}, 'compose_hash': compose_hash(self.baseline),
            'node_info': {'device_id': DEVICE}, 'status': 'running', 'in_progress': False, 'os': {'name': 'dstack-0.6.1'}}))
        self.approved = True
        self.device_approved = True
        self.tamper_stored = False
        self.update_status = 202
        self.events = []
        self.lease = self.tmp / 'lease'
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                payload = json.loads(self.rfile.read(int(self.headers['content-length'])))
                method = payload['method']
                if method == 'eth_chainId':
                    result = '0x2105'
                elif method == 'eth_getStorageAt':
                    impl = fixture.pin['app_implementation' if payload['params'][0].lower() == APP.lower() else 'kms_implementation']
                    result = '0x' + impl[2:].lower().rjust(64, '0')
                elif method == 'eth_call':
                    selector = payload['params'][0]['data'][:10]
                    values = {'0x8da5cb5b': SAFE[2:].lower(), '0x3440a16a': '0',
                              '0xbf8b211b': '1' if fixture.device_approved else '0',
                              '0x2f6622e5': '1' if fixture.approved else '0'}
                    result = '0x' + values[selector].rjust(64, '0')
                    fixture.events.append(selector)
                else:
                    raise AssertionError(method)
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.end_headers()
                self.wfile.write(json.dumps({'jsonrpc': '2.0', 'id': payload['id'], 'result': result}).encode())

            def do_PATCH(self):
                fixture.events.append('PATCH')
                with Path(fixture.env['STUB_LOG']).open('a') as stream:
                    stream.write(json.dumps(['PATCH']) + '\n')
                fixture.assertEqual(fixture.lease.exists(), fixture.env['VARIANT'] != 'migration-import')
                fixture.assertIsNone(self.headers.get('X-Prepare-Only'))
                payload = json.loads(self.rfile.read(int(self.headers['content-length'])))
                fixture.assertNotIn('encrypted_env', payload)
                fixture.assertNotIn('allowed_envs', payload)
                if fixture.update_status != 202:
                    self.send_response(fixture.update_status)
                    self.send_header('Content-Type', 'application/json')
                    self.end_headers()
                    self.wfile.write(b'{"message":"fixture update failed"}')
                    return
                stored = {**fixture.baseline, **payload}
                cvm = json.loads(fixture.cvm.read_text())
                fixture.cvm.with_suffix('.pending.json').write_text(json.dumps({'remaining': 3, 'status': cvm['status']}))
                cvm.update(compose_hash=compose_hash(stored), status='updating', in_progress=True)
                fixture.cvm.write_text(json.dumps(cvm))
                if fixture.tamper_stored:
                    stored['public_logs'] = True
                fixture.stored.write_text(json.dumps(stored))
                self.send_response(202)
                self.send_header('Content-Type', 'application/json')
                self.end_headers()
                self.wfile.write(b'{"correlation_id":"fixture-id"}')

        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(thread.join)
        self.addCleanup(server.shutdown)
        node = self.tmp / 'bin/node'
        node.write_text('''#!/bin/sh
if [ "$2" = identity ]; then printf '3059\\n'
else exec "$FIXTURE_NODE" "$@"; fi
''')
        node.chmod(0o755)
        cli = deploy / 'phala'
        cli.write_text('''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys
args = sys.argv[1:]
def record(args):
    with Path(os.environ['STUB_LOG']).open('a') as stream: stream.write(json.dumps(args) + '\\n')
record(args)
cvm = Path(os.environ['FIXTURE_CVM'])
if args[:2] == ['cvms', 'get']:
    pending = cvm.with_suffix('.pending.json')
    if pending.exists():
        update = json.loads(pending.read_text())
        if update['remaining'] > 0:
            record(['updating', update['remaining']])
            update['remaining'] -= 1
            pending.write_text(json.dumps(update))
        else:
            state = json.loads(cvm.read_text())
            state.update(status=os.environ.get('FIXTURE_SETTLED_STATUS', update['status']), in_progress=False,
                compose_hash=os.environ.get('FIXTURE_SETTLED_HASH', state['compose_hash']))
            cvm.write_text(json.dumps(state))
            pending.unlink()
            record(['settled', state['status'], state['compose_hash']])
    print(cvm.read_text())
elif args[:2] == ['cvms', 'start']:
    if os.environ.get('FIXTURE_START_STATUS'): raise SystemExit(int(os.environ['FIXTURE_START_STATUS']))
    state = json.loads(cvm.read_text())
    assert state['status'] == 'stopped' and not state['in_progress']
    state['status'] = 'running'
    cvm.write_text(json.dumps(state))
    print('{}')
elif args[:2] == ['cvms', 'attestation']:
    print(json.dumps({'tcb_info': {'event_log': [{'event': 'compose-hash', 'event_payload': json.loads(cvm.read_text())['compose_hash']}]}}))
elif args[:2] == ['api', '/cvms/cvm-1/compose_file']: print(Path(os.environ['FIXTURE_STORED']).read_text())
else: raise SystemExit('unexpected Cloud call')
''')
        cli.chmod(0o755)
        pause = deploy / 'instance-pause.sh'
        pause.write_text('''#!/bin/sh
"$FIXTURE_NODE" -e 'require("fs").appendFileSync(process.env.STUB_LOG, JSON.stringify(["maintenance", ...process.argv.slice(1)]) + "\\n")' "$@"
if [ "$1" = pause ]; then
    touch "$FIXTURE_LEASE"
    printf '{"paused_scopes":["mutations"]}\\n'
else
    test -f "$FIXTURE_VERIFIED" || exit 99
    rm "$FIXTURE_LEASE"
    printf '{"paused_scopes":[]}\\n'
fi
''')
        pause.chmod(0o755)
        verifier = deploy / 'verify-attestation.sh'
        verifier.write_text('''#!/bin/sh
if [ "$1" = --partial ]; then
    test -s "$2" && test "$3" = - && touch "$FIXTURE_PARTIAL_VERIFIED"
else
    test -s "$1" && test -s "$2" && touch "$FIXTURE_VERIFIED"
fi
''')
        verifier.chmod(0o755)
        ingress = deploy / 'verify-ingress-evidence.sh'
        ingress.write_text('#!/bin/sh\ntest -f "$FIXTURE_VERIFIED" && printf "fixture certificate evidence\\n"\n')
        ingress.chmod(0o755)
        monitor = deploy / 'upgrade-monitor.py'
        monitor.write_text('''import json
import os
from pathlib import Path
import sys
with Path(os.environ['STUB_LOG']).open('a') as stream: stream.write(json.dumps(['monitor', *sys.argv[1:]]) + '\\n')
''')
        self.env.update(MODE='commit', VARIANT='service', INITIAL_BASE='false', TRANSACTION_HASH='already-registered',
            CVM_ID='cvm-1', APP_ID=APP, DOMAIN='example.invalid', ENVIRONMENT_DIR=str(self.environment), FIXTURE_NODE=shutil.which('node'),
            FIXTURE_CVM=str(self.cvm), FIXTURE_STORED=str(self.stored), FIXTURE_LEASE=str(self.lease),
            FIXTURE_VERIFIED=str(self.tmp / 'verified'), PHALA_CLOUD_API_KEY='fixture-key',
            FIXTURE_PARTIAL_VERIFIED=str(self.tmp / 'partial-verified'),
            PHALA_CLOUD_API_PREFIX=f'http://127.0.0.1:{server.server_port}',
            BASE_RPC_URL=f'http://127.0.0.1:{server.server_port}/rpc')

    def step(self, name):
        result = self.run_step(name)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def prepare(self):
        self.step('Prepare the Base compose and Safe batches')
        self.assertNotIn('PATCH', self.events)

    def verify_and_pause(self):
        self.step('Verify Base approval before pausing')
        self.step('Pause new mutations before upgrading')

    def verify_after_commit(self):
        self.step('Read back and verify the attested compose')
        self.assertTrue((self.tmp / 'verified').exists())
        self.step('Wait for the service')

    def unchanged(self, status='stopped', in_progress=False):
        self.stored.write_text(json.dumps(self.request))
        cvm = json.loads(self.cvm.read_text())
        cvm.update(compose_hash=self.hash, status=status, in_progress=in_progress)
        self.cvm.write_text(json.dumps(cvm))

    def test_approved_prepare_emits_only_missing_device_batch_and_never_writes_cloud(self):
        for allowed in [True, False]:
            self.device_approved = allowed
            self.prepare()
            output = self.work / 'onchain-prepare.json'
            summary = json.loads(Path(str(output) + '.summary.json').read_text())
            self.assertTrue(summary['already_approved'])
            self.assertFalse(Path(str(output) + '.batch.json').exists())
            self.assertEqual(Path(str(output) + '.device-batch.json').exists(), not allowed)

    def test_commit_updates_after_pause_then_waits_and_verifies_before_resume(self):
        selected = {'Prepare the Base compose and Safe batches', 'Verify Base approval before pausing',
            'Pause new mutations before upgrading', 'Deploy (${{ inputs.mode }}) and wait for the CVM',
            'Read back and verify the attested compose', 'Wait for the service',
            'Resume mutations after health checks', 'Verify the certificate evidence'}
        workflow = (ROOT / '.github/workflows/deploy.yml').read_text()
        for name in re.findall(r'^      - name: (.*)$', workflow, re.MULTILINE):
            if name in selected:
                self.step(name)
        self.assertEqual(self.events.count('PATCH'), 1)
        self.assertEqual(self.events[self.events.index('PATCH') - 1], '0x2f6622e5')
        self.assertTrue((self.tmp / 'verified').exists())
        self.assertFalse(self.lease.exists())
        self.assertTrue((self.work / 'ingress-evidence.txt').exists())
        self.assertIn(['maintenance', 'pause', self.env['PUBLIC_URL'], '1-1'], self.calls())
        self.assertTrue(any(call[:2] == ['monitor', self.env['PUBLIC_URL']] for call in self.calls()))
        self.assertFalse(any(call[:2] == ['cvms', 'start'] for call in self.calls()))

    def test_stopped_changed_commit_applies_then_starts_before_verification(self):
        self.prepare()
        cvm = json.loads(self.cvm.read_text())
        cvm['status'] = 'stopped'
        self.cvm.write_text(json.dumps(cvm))
        self.verify_and_pause()
        self.step('Deploy (${{ inputs.mode }}) and wait for the CVM')
        self.verify_after_commit()
        self.step('Resume mutations after health checks')
        self.step('Verify the certificate evidence')
        calls = [call for call in self.calls() if call[0] != 'monitor']
        applied = calls.index(['PATCH'])
        started = calls.index(['cvms', 'start', 'cvm-1', '--json'])
        attested = calls.index(['cvms', 'attestation', 'cvm-1', '--json'])
        self.assertLess(applied, started)
        self.assertLess(started, attested)
        self.assertEqual(calls[applied + 1:started], [
            ['cvms', 'get', 'cvm-1', '--json'], ['updating', 3],
            ['cvms', 'get', 'cvm-1', '--json'], ['updating', 2],
            ['cvms', 'get', 'cvm-1', '--json'], ['updating', 1],
            ['cvms', 'get', 'cvm-1', '--json'], ['settled', 'stopped', self.hash]])
        self.assertEqual(calls[started + 1], ['cvms', 'get', 'cvm-1', '--json'])
        self.assertEqual(json.loads(self.cvm.read_text())['compose_hash'], self.hash)
        self.assertFalse(self.lease.exists())

    def test_stopped_changed_commit_rejects_another_settled_state_before_start(self):
        self.prepare()
        self.verify_and_pause()
        for status in ['running', 'error']:
            with self.subTest(status=status):
                cvm = json.loads(self.cvm.read_text())
                cvm.update(status='stopped', in_progress=False, compose_hash=compose_hash(self.baseline))
                self.cvm.write_text(json.dumps(cvm))
                self.env['FIXTURE_SETTLED_STATUS'] = status
                result = self.run_step('Deploy (${{ inputs.mode }}) and wait for the CVM')
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn('must settle stopped', result.stderr)
                self.assertFalse(any(call[:2] == ['cvms', 'start'] for call in self.calls()))
                self.assertFalse((self.tmp / 'verified').exists())

    def test_stopped_changed_commit_times_out_on_the_wrong_settled_hash_before_start(self):
        self.prepare()
        cvm = json.loads(self.cvm.read_text())
        cvm['status'] = 'stopped'
        self.cvm.write_text(json.dumps(cvm))
        self.verify_and_pause()
        self.env.update(FIXTURE_SETTLED_HASH='cd' * 32, CVM_WAIT_SECONDS='2')
        result = self.run_step('Deploy (${{ inputs.mode }}) and wait for the CVM')
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('deadline expired', result.stderr)
        self.assertIn(['settled', 'stopped', 'cd' * 32], self.calls())
        self.assertFalse(any(call[:2] == ['cvms', 'start'] for call in self.calls()))
        self.assertFalse((self.tmp / 'verified').exists())

    def test_changed_error_or_busy_commit_fails_before_update_or_start(self):
        self.prepare()
        for status, busy in [('error', False), ('starting', False), ('stopped', True), ('running', True)]:
            with self.subTest(status=status, busy=busy):
                cvm = json.loads(self.cvm.read_text())
                cvm.update(status=status, in_progress=busy)
                self.cvm.write_text(json.dumps(cvm))
                result = self.run_step('Deploy (${{ inputs.mode }}) and wait for the CVM')
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('no operation in progress', result.stderr)
                self.assertNotIn('PATCH', self.events)
                self.assertFalse(any(call[:2] == ['cvms', 'start'] for call in self.calls()))

    def test_failed_changed_update_does_not_start_stopped_cvm(self):
        self.prepare()
        cvm = json.loads(self.cvm.read_text())
        cvm['status'] = 'stopped'
        self.cvm.write_text(json.dumps(cvm))
        self.verify_and_pause()
        self.update_status = 400
        result = self.run_step('Deploy (${{ inputs.mode }}) and wait for the CVM')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('PATCH', self.events)
        self.assertFalse(any(call[:2] == ['cvms', 'start'] for call in self.calls()))
        self.assertEqual(json.loads(self.cvm.read_text())['status'], 'stopped')

    def test_start_failure_fails_commit_before_attestation_or_resume(self):
        self.prepare()
        cvm = json.loads(self.cvm.read_text())
        cvm['status'] = 'stopped'
        self.cvm.write_text(json.dumps(cvm))
        self.verify_and_pause()
        self.env['FIXTURE_START_STATUS'] = '42'
        result = self.run_step('Deploy (${{ inputs.mode }}) and wait for the CVM')
        self.assertEqual(result.returncode, 42, result.stdout + result.stderr)
        self.assertIn('PATCH', self.events)
        self.assertFalse(any(call[:2] == ['cvms', 'attestation'] for call in self.calls()))
        self.assertFalse((self.tmp / 'verified').exists())
        self.assertTrue(self.lease.exists())

    def test_import_commit_never_contacts_public_url_and_leaves_full_acceptance_pending(self):
        self.env['VARIANT'] = 'migration-import'
        self.env['STUB_INFO_STATUS'] = '42'
        self.prepare()
        cvm = json.loads(self.cvm.read_text())
        cvm['status'] = 'stopped'
        self.cvm.write_text(json.dumps(cvm))
        self.verify_and_pause()
        self.assertFalse(self.lease.exists())
        self.step('Deploy (${{ inputs.mode }}) and wait for the CVM')
        self.step('Read back and verify the attested compose')
        self.assertTrue((self.tmp / 'partial-verified').exists())
        self.assertFalse((self.tmp / 'verified').exists())
        self.assertTrue(any(call[:2] == ['cvms', 'start'] for call in self.calls()))
        self.assertFalse(any(call[0] in ['maintenance', 'monitor', 'health', 'info'] for call in self.calls()))
        self.assertFalse((self.work / 'info.json').exists())
        self.assertFalse((self.work / 'ingress-evidence.txt').exists())
        pending = (self.work / 'acceptance-pending.txt').read_text()
        self.assertIn('Full OS/vm_config acceptance is pending', pending)
        self.assertIn('import-info.json and recipient.json', pending)
        self.assertIn('step 6', pending)
        self.assertIn(pending, (self.tmp / 'summary').read_text())
        self.assertIn('enabled=false', (self.tmp / 'output').read_text())

    def test_import_skips_steps_that_require_an_origin(self):
        workflow = (ROOT / '.github/workflows/deploy.yml').read_text()
        for name in ['Print the owner\'s DNS records', 'Wait for the service', 'Resume mutations after health checks',
                     'Verify the certificate evidence', 'Clear maintenance after a failed upgrade', 'Report observed unavailability']:
            with self.subTest(step=name):
                block = workflow.split(f'      - name: {name}\n', 1)[1].split('      - name:', 1)[0]
                condition = re.search(r'^        if: (.+)$', block, re.MULTILINE)[1]
                self.assertIn("inputs.variant != 'migration-import'", condition)
        self.env['VARIANT'] = 'migration-import'
        self.step("Print the owner's sealing step")
        self.assertIn('step 5', (self.tmp / 'summary').read_text())
        self.assertNotIn(self.env['PUBLIC_URL'], (self.tmp / 'summary').read_text())

    def test_stopped_unchanged_commit_starts_and_verifies_without_a_lease(self):
        self.unchanged('running')
        self.prepare()
        self.unchanged('stopped')
        self.verify_and_pause()
        self.assertFalse(self.lease.exists())
        self.step('Deploy (${{ inputs.mode }}) and wait for the CVM')
        self.assertTrue(any(call[:2] == ['cvms', 'start'] for call in self.calls()))
        self.assertNotIn('PATCH', self.events)
        self.verify_after_commit()
        self.step('Verify the certificate evidence')
        self.assertIn('enabled=false', (self.tmp / 'output').read_text())
        self.assertIn('no maintenance lease: CVM was stopped; started the already-stored approved compose',
                      (self.tmp / 'summary').read_text())

    def test_running_unchanged_commit_keeps_the_lease_and_does_not_start(self):
        self.unchanged('running')
        self.prepare()
        self.verify_and_pause()
        self.assertTrue(self.lease.exists())
        self.step('Deploy (${{ inputs.mode }}) and wait for the CVM')
        self.assertFalse(any(call[:2] == ['cvms', 'start'] for call in self.calls()))
        self.verify_after_commit()
        self.step('Resume mutations after health checks')
        self.step('Verify the certificate evidence')

    def test_unchanged_error_or_busy_cvm_fails_before_start(self):
        for status, busy in [('error', False), ('starting', False), ('stopped', True), ('running', True)]:
            self.unchanged('running')
            self.prepare()
            self.unchanged(status, busy)
            result = self.run_step('Pause new mutations before upgrading')
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('no operation in progress', result.stdout)
            self.assertFalse(self.lease.exists())
            self.assertFalse(any(call[:2] == ['cvms', 'start'] for call in self.calls()))

    def test_changed_approval_stops_commit_before_cloud_update(self):
        self.prepare()
        self.verify_and_pause()
        self.approved = False
        result = self.run_step('Deploy (${{ inputs.mode }}) and wait for the CVM')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('compose hash is not approved', result.stderr)
        self.assertNotIn('PATCH', self.events)

    def test_stored_compose_mismatch_prevents_attestation_acceptance(self):
        self.prepare()
        self.verify_and_pause()
        self.tamper_stored = True
        self.step('Deploy (${{ inputs.mode }}) and wait for the CVM')
        result = self.run_step('Read back and verify the attested compose')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('compose hash differs', result.stderr)
        self.assertFalse((self.tmp / 'verified').exists())
        self.assertTrue(self.lease.exists())


if __name__ == '__main__':
    unittest.main()
