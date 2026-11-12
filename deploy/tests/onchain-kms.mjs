#!/usr/bin/env node
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { getComposeHash } from '../tools/node_modules/@phala/cloud/dist/index.mjs';
import { canonical, composeHash, compareHash, batch, prepare, verify, deployment, identity, verifyProviderEvent, config, slot, appEvents, kmsEvents } from '../tools/onchain-kms.mjs';
import { encodeEventTopics, toEventSelector, toFunctionSelector } from '../tools/node_modules/viem/_esm/index.js';

const safe = '0x8AB55148944F837040bbB887f8A5005f70FF7700';
const app = '0x1111111111111111111111111111111111111111';
const hash = 'ab'.repeat(32), device = 'cd'.repeat(32);
test('production v0.13.6 app-compose reproduces the attested raw bytes and SHA-256', () => {
  // Public compose only, from production Deploy run 41686087726. No instance/node identifiers.
  const raw = readFileSync(new URL('./fixtures/production-app-compose.json', import.meta.url), 'utf8');
  const compose = JSON.parse(raw);
  assert.equal(canonical(compose), raw);
  assert.equal(composeHash(compose), 'ed7dc915032602b6f2be6aa738c622d36828613f386be01e81efabcd8791e4e4');
  assert.equal(composeHash(compose), getComposeHash(compose));
});
test('hash vectors match pinned official export, preserving arrays and UTF-8', () => {
  for (const compose of [{z: {b: 'é雪', a: 3}, a: [false, null, {z: 1, a: 2}]}, {runner: 'bash', docker_compose_file: 'x', pre_launch_script: ''}]) {
    assert.equal(composeHash(compose), getComposeHash(compose));
    assert.equal(compareHash(compose, `0x${composeHash(compose)}`), composeHash(compose));
    assert.throws(() => compareHash(compose, hash), /differs/);
  }
});
test('Safe calldata matches independently recorded Solidity selectors and ABI words', () => {
  const words = value => value.padStart(64, '0');
  const vectors = [
    ['deployAndRegisterApp', '0x8618169d', words(safe.slice(2).toLowerCase()) + words('0') + words('0') + device + hash],
    ['addComposeHash', '0xdfc77223', hash],
    ['removeComposeHash', '0x67b3f22c', hash],
    ['addDevice', '0x2a819728', hash],
  ];
  for (const [method, selector, args] of vectors) {
    const result = batch(safe, method === 'deployAndRegisterApp' ? config.kms_auth : app, method, hash, device);
    assert.equal(result.chainId, '8453');
    assert.equal(result.meta.createdFromSafeAddress, safe);
    assert.equal(result.transactions[0].data, selector + args);
    assert.equal(result.transactions[0].value, '0');
    assert.match(result.meta.checksum, /^0x[0-9a-f]{64}$/);
  }
  assert.throws(() => batch(safe, app, 'addComposeHash', 'bad'), /hex/);
});
function stub(overrides = {}) {
  return {
    async getStorageAt({address, slot: actualSlot}) {
      assert.equal(actualSlot, slot);
      const impl = address === app ? config.app_implementation : config.kms_implementation;
      return '0x' + '0'.repeat(24) + impl.slice(2).toLowerCase();
    },
    async readContract({functionName}) {
      return ({owner: safe, allowedComposeHashes: true, allowedDeviceIds: true, allowAnyDevice: false,
        kmsInfo: ['0x01', '0x3059', '0x', '0x']})[functionName];
    }, ...overrides,
  };
}
test('verification rejects incorrect owner, hash/device policy and either proxy upgrade', async () => {
  assert.equal((await verify(stub(), app, safe, hash, device)).passed, true);
  for (const [name, value] of [['owner', app], ['allowedComposeHashes', false], ['allowedDeviceIds', false], ['allowAnyDevice', true]]) {
    const base = stub();
    await assert.rejects(verify(stub({readContract: args => args.functionName === name ? value : base.readContract(args)}), app, safe, hash, device));
  }
  for (const target of [app, config.kms_auth]) {
    const base = stub();
    await assert.rejects(verify(stub({getStorageAt: args => args.address === target ? '0x' + '0'.repeat(64) : base.getStorageAt(args)}), app, safe, hash, device), /unreviewed/);
  }
  assert.equal(await identity(stub()), '3059');
});
test('deployment requires success, exact KmsAuth emitter and exactly one matching Safe event', async () => {
  const topics = encodeEventTopics({abi: [{type: 'event', name: 'AppDeployedViaFactory', inputs: [
    {name: 'appId', type: 'address', indexed: true}, {name: 'deployer', type: 'address', indexed: true}]}],
    eventName: 'AppDeployedViaFactory', args: {appId: app, deployer: safe}});
  const log = {address: config.kms_auth, topics, data: '0x'};
  const run = receipt => deployment({getTransactionReceipt: async () => receipt}, '0x' + hash, safe);
  assert.equal(await run({status: 'success', logs: [log]}), app);
  for (const receipt of [{status: 'reverted', logs: [log]}, {status: 'success', logs: []},
    {status: 'success', logs: [log, log]}, {status: 'success', logs: [{...log, address: app}]}]) {
    await assert.rejects(run(receipt));
  }
});

test('replayed key-provider event binds Base CA identity and rejects wrong/missing/duplicate events', () => {
  const event = provider => ({event: 'key-provider', event_payload: Buffer.from(JSON.stringify(provider)).toString('hex')});
  const attestation = events => ({tcb_info: {event_log: events}});
  const good = event({name: 'kms', id: '3059'});
  verifyProviderEvent(attestation([good]), '3059');
  for (const events of [[], [good, good], [event({name: 'local-sgx', id: '3059'})],
    [event({name: 'kms', id: '3058'})], [{event: 'key-provider', event_payload: 'zz'}]]) {
    assert.throws(() => verifyProviderEvent(attestation(events), '3059'));
  }
});

test('approved hash preparation reads policy without any Cloud write, including when a device needs approval', async () => {
  for (const deviceApproved of [true, false]) {
    const base = stub();
    const reads = [];
    const state = stub({readContract: args => {
      reads.push(args.functionName);
      return args.functionName === 'allowedDeviceIds' ? deviceApproved : base.readContract(args);
    }});
    const result = await prepare(state, {patch: () => assert.fail('approved prepare must not write Cloud')},
      'cvm-test', {docker_compose_file: 'services: {}'}, app, safe, device);
    assert.equal(result.already_approved, true);
    assert.equal(result.device_approved, deviceApproved);
    assert.equal(result.token, undefined);
    assert.equal(reads.at(-1), 'allowedComposeHashes');
  }
});

test('Cloud prepare checks approval immediately before PATCH, handles 465 privately and fails hard on 2xx', async () => {
  const {createServer} = await import('node:http');
  const {mkdtemp, writeFile, readFile, stat, rm} = await import('node:fs/promises');
  const {tmpdir} = await import('node:os');
  const {join} = await import('node:path');
  const {spawn} = await import('node:child_process');
  const calls = [];
  const request = {docker_compose_file: 'services: {}', pre_launch_script: 'true', public_logs: false, public_sysinfo: false,
    allowed_envs: ['SENTRY_DSN'], port_policy: {ports: []}};
  const expectedHash = composeHash(request);
  let accepted = false;
  const server = createServer(async (request, response) => {
    let body = ''; for await (const chunk of request) body += chunk;
    if (request.url === '/rpc') {
      const rpc = JSON.parse(body);
      let result;
      if (rpc.method === 'eth_chainId') result = '0x2105';
      else if (rpc.method === 'eth_getStorageAt') {
        const impl = rpc.params[0].toLowerCase() === app.toLowerCase() ? config.app_implementation : config.kms_implementation;
        result = '0x' + '0'.repeat(24) + impl.slice(2).toLowerCase();
      } else if (rpc.method === 'eth_call') {
        const selector = rpc.params[0].data.slice(0, 10);
        calls.push(selector);
        result = '0x' + (selector === toFunctionSelector('owner()') ? safe.slice(2).toLowerCase().padStart(64, '0') :
          (selector === toFunctionSelector('allowedDeviceIds(bytes32)') ? '1' : '0').padStart(64, '0'));
      } else assert.fail(`unexpected RPC ${rpc.method}`);
      response.writeHead(200, {'content-type': 'application/json'});
      response.end(JSON.stringify({jsonrpc: '2.0', id: rpc.id, result}));
      return;
    }
    calls.push({method: request.method, url: request.url, prepare: request.headers['x-prepare-only'], body: JSON.parse(body)});
    if (accepted) {
      response.writeHead(202, {'content-type': 'application/json'});
      response.end(JSON.stringify({correlation_id: 'fixture-id'}));
      return;
    }
    const fields = {compose_hash: expectedHash,
      app_id: app.slice(2), device_id: device, commit_token: 'private-test-token',
      kms_info: {id: 'kms-test', slug: 'base', url: 'https://kms.example.com', version: '1', chain_id: 8453,
        kms_contract_address: config.kms_auth, gateway_app_id: null}};
    response.writeHead(465, {'content-type': 'application/json'});
    response.end(JSON.stringify({detail: {message: 'approval needed', details: Object.entries(fields).map(([field, value]) => ({field, value}))}}));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const directory = await mkdtemp(join(tmpdir(), 'onchain-token-test-'));
  try {
    await writeFile(join(directory, 'request.json'), JSON.stringify(request));
    const run = async () => {
      const child = spawn(process.execPath, [new URL('../tools/onchain-kms.mjs', import.meta.url).pathname,
        'cloud-prepare', 'cvm-test', join(directory, 'request.json'), join(directory, 'result.json'), app, safe, device],
        {env: {...process.env, PHALA_CLOUD_API_KEY: 'fixture-api-key', PHALA_CLOUD_API_PREFIX: `http://127.0.0.1:${server.address().port}`,
          BASE_RPC_URL: `http://127.0.0.1:${server.address().port}/rpc`}});
      let output = ''; child.stdout.on('data', chunk => { output += chunk; }); child.stderr.on('data', chunk => { output += chunk; });
      const code = await new Promise(resolve => child.on('close', resolve));
      return {code, output};
    };
    const first = await run();
    assert.equal(first.code, 0, first.output);
    assert.equal(first.output.includes('private-test-token'), false);
    assert.equal(JSON.parse(await readFile(join(directory, 'result.json'), 'utf8')).token, 'private-test-token');
    assert.equal((await stat(join(directory, 'result.json'))).mode & 0o777, 0o600);
    assert.equal(calls.at(-2), toFunctionSelector('allowedComposeHashes(bytes32)'));
    assert.deepEqual(calls.at(-1), {method: 'PATCH', url: '/cvms/cvm-test', prepare: 'true', body: {
      docker_compose_file: request.docker_compose_file, pre_launch_script: request.pre_launch_script,
      public_logs: false, public_sysinfo: false}});
    accepted = true;
    const second = await run();
    assert.equal(second.code, 1);
    assert.match(second.output, /may have been applied without a pause; stop, inspect the stored compose and attestation, and run commit to verify/);
  } finally {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    await rm(directory, {recursive: true, force: true});
  }
});

test('every monitored governance topic occurs in the pinned deployed implementation bytecode', () => {
  const fixture = JSON.parse(readFileSync(new URL('./fixtures/base-kms-bytecode.json', import.meta.url)));
  assert.equal(fixture.chain_id, 8453);
  assert.equal(fixture.implementations.app.address, config.app_implementation);
  assert.equal(fixture.implementations.kms.address, config.kms_implementation);
  for (const [name, events] of [['app', appEvents], ['kms', kmsEvents]]) {
    for (const event of events) assert.ok(fixture.implementations[name].code.includes(toEventSelector(event).slice(2)), `${name}: ${event}`);
    assert.equal(fixture.implementations[name].code.includes(toEventSelector('PolicyChanged(address,bytes32,bytes32,bool)').slice(2)), false);
  }
  // Other tenants register apps on the shared KmsAuth daily; that must not fail governance review.
  assert.equal(kmsEvents.includes('AppRegistered(address)'), false);
});
