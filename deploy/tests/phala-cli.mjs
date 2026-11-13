#!/usr/bin/env node
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { promisify } from 'node:util';
import { secp256k1 } from '../tools/node_modules/@noble/curves/secp256k1.js';
import { x25519 } from '../tools/node_modules/@noble/curves/ed25519.js';
import { keccak256, toBytes } from '../tools/node_modules/viem/_esm/index.js';
import { createClient, getCvmInfo, patchCvm, commitCvmUpdate, provisionCvm } from '../tools/node_modules/@phala/cloud/dist/index.mjs';

test('locked CLI and SDK retain the Base encryption key and deployment response contracts', {timeout: 60000}, async context => {
  const directory = await mkdtemp(join(tmpdir(), 'phala-cli-'));
  context.after(() => rm(directory, {recursive: true, force: true}));
  const app = '11'.repeat(20);
  const id = 'cvm-test';
  const kmsKey = secp256k1.utils.randomPrivateKey();
  const kmsPubkey = Buffer.from(secp256k1.getPublicKey(kmsKey)).toString('hex');
  const envPubkey = x25519.getPublicKey(x25519.utils.randomPrivateKey());
  const message = Buffer.concat([Buffer.from('dstack-env-encrypt-pubkey:'), Buffer.from(app, 'hex'), envPubkey]);
  const signature = secp256k1.sign(toBytes(keccak256(message)), kmsKey);
  const pubkeyResponse = {public_key: Buffer.from(envPubkey).toString('hex'),
    signature: signature.toCompactHex() + signature.recovery.toString(16).padStart(2, '0')};
  const cvm = {id, name: 'test-cvm', app_id: app, status: 'running', in_progress: false,
    resource: {vcpu: 1, memory_in_mb: 2048, disk_in_gb: 20}, gateway: {base_domain: 'example.com'},
    kms_type: 'base', kms_info: {chain_id: 8453, k256_pubkey: kmsPubkey}, logs: {}};
  const vm = {id, name: cvm.name, app_id: app, status: 'starting', teepod_id: 1,
    vm_uuid: null, instance_id: null, vcpu: 1, memory: 2048, disk_size: 20,
    created_at: '2026-11-12T00:00:00Z', encrypted_env_pubkey: null};
  const calls = [];
  let badSignature = false;
  const server = createServer(async (request, response) => {
    try {
      let body = '';
      for await (const chunk of request) body += chunk;
      const path = new URL(request.url, 'http://localhost').pathname;
      calls.push({path, method: request.method, version: request.headers['x-phala-version'],
        prepare: request.headers['x-prepare-only'], body: body ? JSON.parse(body) : undefined});
      const json = (value, status = 200) => {
        response.writeHead(status, {'content-type': 'application/json'});
        response.end(JSON.stringify(value));
      };
      if (path === `/cvms/${id}` && request.method === 'GET') json(cvm);
      else if (path === `/cvms/${id}` && request.method === 'PATCH') json({status: 'no_change'});
      else if (path === `/cvms/${id}/start`) json(vm);
      else if (path === `/cvms/${id}/commit-update`) json({status: 'in_progress', correlation_id: 'commit-id'});
      else if (path === `/kms/base/pubkey/${app}`) json({...pubkeyResponse,
        signature: badSignature ? '00' : pubkeyResponse.signature});
      else if (path === `/cvms/${id}/envs`) json({status: 'in_progress', message: 'accepted',
        correlation_id: 'env-id', allowed_envs_changed: false});
      else if (path === '/serial') {
        response.writeHead(200, {'content-type': 'text/plain'});
        response.end('booted\n');
      } else if (path === '/empty') response.writeHead(202).end();
      else if (path === '/cvms' && request.method === 'POST') {
        if (calls.at(-1).body.token) json({...vm, vm_uuid: id, teepod: null, user_id: null,
          base_image: null, docker_compose_file: 'services: {}'});
        else json({app_id: app, compose_hash: 'ab'.repeat(32), token: 'test-create-token'});
      } else json({error: 'unexpected request'}, 500);
    } catch {
      response.writeHead(500).end();
    }
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  context.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve); }));
  const baseURL = `http://127.0.0.1:${server.address().port}`;
  cvm.logs.serial = `${baseURL}/serial`;
  const client = createClient({baseURL, apiKey: 'test-token'});
  const run = (...args) => promisify(execFile)(new URL('../phala', import.meta.url).pathname, args,
    {timeout: 15000, env: {...process.env, PHALA_CLOUD_API_KEY: 'test-token', PHALA_CLOUD_API_PREFIX: baseURL,
      PHALA_CLOUD_DIR: directory, PHALA_CLOUD_WORKSPACE: '', PRIVATE_KEY: ''}});

  await context.test('CVM JSON keeps k256_pubkey and the current memory field', async () => {
    const sdk = await getCvmInfo(client, {id});
    const cli = JSON.parse((await run('cvms', 'get', id, '--json')).stdout);
    for (const result of [sdk, cli]) {
      assert.equal(result.kms_info.k256_pubkey, kmsPubkey);
      assert.equal(result.kms_info.chain_id, 8453);
      assert.equal(result.resource.memory_in_mb, 2048);
      assert.equal(result.id, id);
    }
    assert.equal(calls.at(-1).version, '2026-11-11');
  });
  await context.test('envs update -e verifies a signed Base pubkey before encrypting', async () => {
    await run('envs', 'update', id, '-e', 'TEST_VALUE=fixture-value');
    const update = calls.at(-1);
    assert.equal(update.path, `/cvms/${id}/envs`);
    assert.deepEqual(update.body.env_keys, ['TEST_VALUE']);
    assert.match(update.body.encrypted_env, /^(?:[0-9a-f]{2})+$/i);
    assert.equal(JSON.stringify(update.body).includes('fixture-value'), false);
    badSignature = true;
    const count = calls.filter(call => call.path.endsWith('/envs')).length;
    await assert.rejects(run('envs', 'update', id, '-e', 'TEST_VALUE=fixture-value'), error =>
      /Invalid encryption public key signature/.test(error.stdout + error.stderr));
    assert.equal(calls.filter(call => call.path.endsWith('/envs')).length, count);
  });
  await context.test('start wraps the VM, serial logs remain text, raw api emits JSON and empty bodies', async () => {
    const start = JSON.parse((await run('cvms', 'start', id, '--json')).stdout);
    assert.equal(start.cvm.id, id);
    assert.equal(start.message, 'CVM start requested');
    assert.equal(calls.at(-1).method, 'POST');
    assert.equal(calls.at(-1).path, `/cvms/${id}/start`);
    assert.equal((await run('logs', '--cvm-id', id, '--serial')).stdout, 'booted\n');
    await assert.rejects(run('logs', '--cvm-id', id, '--serial', '--since', '1m'), error =>
      /not supported for serial/.test(error.stdout + error.stderr));
    const raw = JSON.parse((await run('api', `/cvms/${id}`)).stdout);
    assert.equal(raw.kms_info.k256_pubkey, kmsPubkey);
    assert.equal((await run('api', '/empty', '-X', 'PATCH', '-F', 'os_image_name=dstack-0.6.1')).stdout.trim(), '');
    assert.deepEqual(calls.at(-1).body, {os_image_name: 'dstack-0.6.1'});
  });
  await context.test('SDK normalizes no_change and preserves prepare-only and token commit wire fields', async () => {
    for (const prepareOnly of [true, false]) {
      const result = await patchCvm(client, {id, docker_compose_file: 'services: {}', prepareOnly});
      assert.deepEqual(result, {requiresOnChainHash: false, correlationId: ''});
      assert.equal(calls.at(-1).prepare, prepareOnly ? 'true' : undefined);
      assert.deepEqual(calls.at(-1).body, {docker_compose_file: 'services: {}'});
    }
    assert.deepEqual(await commitCvmUpdate(client, {id, token: 'test-token', composeHash: 'ab'.repeat(32),
      transactionHash: 'already-registered'}), {correlationId: 'commit-id', status: 'in_progress'});
    assert.deepEqual(calls.at(-1).body, {token: 'test-token', compose_hash: 'ab'.repeat(32), transaction_hash: 'already-registered'});
  });
  await context.test('SDK create prepares at /cvms, retaining string manifest version and removing the gateway alias', async () => {
    const provision = await provisionCvm(client, {name: cvm.name, compose_file: {docker_compose_file: 'services: {}',
      manifest_version: '3', gateway_enabled: true, tproxy_enabled: true}});
    assert.equal(provision.token, 'test-create-token');
    assert.equal(calls.at(-1).path, '/cvms');
    assert.equal(calls.at(-1).body.compose_file.manifest_version, '3');
    assert.equal(calls.at(-1).body.compose_file.gateway_enabled, true);
    assert.equal('tproxy_enabled' in calls.at(-1).body.compose_file, false);
  });
  await context.test('deploy accepts the workflow provisioning flags and sends the physical node name', async () => {
    const compose = join(directory, 'compose.yml');
    const env = join(directory, 'env');
    await writeFile(compose, 'services:\n  test:\n    image: busybox\n');
    await writeFile(env, '');
    const before = calls.length;
    const result = await run('deploy', '--json', '--node', 'prod5', '--name', cvm.name,
      '--compose', compose, '--pre-launch-script', new URL('../phala-cloud-pre-launch.sh', import.meta.url).pathname,
      '-e', env, '--instance-type', 'tdx.medium', '--fs', 'ext4', '--kms', 'phala', '--image', 'dstack-0.6.1',
      '--no-dev-os', '--no-public-logs', '--no-public-sysinfo', '--public-tcbinfo', '--secure-time');
    assert.equal(JSON.parse(result.stdout.slice(result.stdout.indexOf('{'))).vm_uuid, id);
    const provision = calls.slice(before).find(call => call.path === '/cvms' && call.body?.node);
    assert.equal(provision.body.node, 'prod5');
    assert.equal('node_id' in provision.body, false);
  });
});
