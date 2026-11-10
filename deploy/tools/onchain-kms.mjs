#!/usr/bin/env node
// Read-only Base governance checks and offline calldata. viem is locked by package-lock.json.
import { createClient, patchCvm } from '@phala/cloud';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { createPublicClient, http, parseAbi, encodeFunctionData, decodeEventLog,
  getAddress, keccak256, stringToHex, toEventSelector } from 'viem';

export const config = JSON.parse(readFileSync(new URL('../onchain-kms.json', import.meta.url)));
export const slot = '0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc';
export const abi = parseAbi([
  'function deployAndRegisterApp(address initialOwner,bool disableUpgrades,bool allowAnyDevice,bytes32 deviceId,bytes32 composeHash) returns (address)',
  'function addComposeHash(bytes32 composeHash)', 'function removeComposeHash(bytes32 composeHash)',
  'function addDevice(bytes32 deviceId)', 'function owner() view returns (address)',
  'function allowedComposeHashes(bytes32 hash) view returns (bool)',
  'function allowedDeviceIds(bytes32 device) view returns (bool)',
  'function allowAnyDevice() view returns (bool)',
  'function kmsInfo() view returns (bytes k256Pubkey,bytes caPubkey,bytes quote,bytes eventlog)',
  'event AppDeployedViaFactory(address indexed appId,address indexed deployer)',
]);
function requireThat(condition, message) { if (!condition) throw new Error(message); }
export function hex(value, bytes) {
  requireThat(typeof value === 'string' && new RegExp(`^(0x)?[0-9a-fA-F]{${bytes * 2}}$`).test(value), `expected ${bytes}-byte hex`);
  return `0x${value.replace(/^0x/, '').toLowerCase()}`;
}
export function address(value) { return getAddress(hex(value, 20)); }
export function canonical(compose) {
  const sort = value => Array.isArray(value) ? value.map(sort) : value && typeof value === 'object'
    ? Object.fromEntries(Object.keys(value).sort().map(key => [key, sort(value[key])])) : value;
  // Empirically matches production v0.13.6's attested compose_file bytes. Current
  // @phala/cloud exports @phala/dstack-sdk's sorted, compact JSON (no preprocessing).
  return JSON.stringify(sort(compose));
}
export function composeHash(compose) { return createHash('sha256').update(canonical(compose)).digest('hex'); }
export function compareHash(compose, returned) {
  const local = composeHash(compose);
  requireThat(hex(returned, 32) === `0x${local}`, `Phala compose hash differs from local SHA-256 ${local}; refusing approval/commit`);
  return local;
}
// Same Transaction Builder BatchFile/checksum serialization as sdk/js-server/src/sweeps.ts.
function serialize(value) {
  if (Array.isArray(value)) return `[${value.map(serialize).join(',')}]`;
  if (value && typeof value === 'object') {
    const keys = Object.keys(value).sort();
    return `{${JSON.stringify(keys)}${keys.map(key => `${serialize(value[key])},`).join('')}}`;
  }
  return JSON.stringify(value ?? null);
}
export function batch(safe, target, method, value, device) {
  const args = method === 'deployAndRegisterApp' ? [address(safe), false, false, hex(device, 32), hex(value, 32)] : [hex(value, 32)];
  requireThat(['deployAndRegisterApp', 'addComposeHash', 'removeComposeHash', 'addDevice'].includes(method), 'unsupported Safe call');
  const result = { version: '1.0', chainId: '8453', createdAt: Date.now(),
    meta: { name: `Phala Pay ${method}`, description: 'Review app, device, compose and Base chain before signing.', createdFromSafeAddress: address(safe) },
    transactions: [{ to: address(target), value: '0', data: encodeFunctionData({abi, functionName: method, args}) }] };
  result.meta.checksum = keccak256(stringToHex(serialize({...result, meta: {...result.meta, name: null}})));
  return result;
}
export function verifyProviderEvent(attestation, expected) {
  // dstack-util/src/system_setup.rs emit_key_provider_info serializes {name,id}
  // as JSON UTF-8 in the runtime event; the quote verifier must replay it first.
  const events = attestation.tcb_info?.event_log?.filter(event => event.event === 'key-provider');
  requireThat(events?.length === 1, 'expected one replayed key-provider event');
  const payload = events[0].event_payload.replace(/^0x/, '');
  requireThat(/^(?:[0-9a-fA-F]{2})+$/.test(payload), 'invalid key-provider event hex');
  const provider = JSON.parse(Buffer.from(payload, 'hex').toString('utf8'));
  requireThat(provider.name === 'kms' && provider.id === expected, 'key-provider event differs from Base KMS identity');
}
export async function client() {
  const rpc = process.env.BASE_RPC_URL || 'https://base-rpc.publicnode.com';
  const c = createPublicClient({transport: http(rpc, {timeout: 20000, retryCount: 2})});
  requireThat(await c.getChainId() === 8453, 'RPC must be Base (8453)');
  return c;
}
async function implementation(c, target, expected) {
  const value = await c.getStorageAt({address: target, slot});
  requireThat(value?.toLowerCase() === `0x${'0'.repeat(24)}${address(expected).slice(2).toLowerCase()}`, `unreviewed implementation at ${target}`);
}
export async function identity(c) {
  await implementation(c, config.kms_auth, config.kms_implementation);
  const info = await c.readContract({address: config.kms_auth, abi, functionName: 'kmsInfo'});
  requireThat(info[1] && info[1] !== '0x', 'Base KMS has no CA identity');
  return info[1].slice(2).toLowerCase();
}
export async function verify(c, app, safe, hash, device) {
  app = address(app); safe = address(safe); hash = hex(hash, 32); device = hex(device, 32);
  await implementation(c, config.kms_auth, config.kms_implementation);
  await implementation(c, app, config.app_implementation);
  const read = (functionName, args = []) => c.readContract({address: app, abi, functionName, args});
  requireThat(address(await read('owner')) === safe, 'DstackApp owner differs from expected Safe');
  requireThat(await read('allowedComposeHashes', [hash]), 'compose hash is not approved');
  requireThat(await read('allowAnyDevice') === false, 'allowAnyDevice must be false');
  requireThat(await read('allowedDeviceIds', [device]), 'device is not approved');
  return {passed: true, chain_id: 8453, app, safe, compose_hash: hash, device_id: device,
    app_implementation: config.app_implementation, kms_implementation: config.kms_implementation};
}
export async function deployment(c, tx, safe) {
  tx = hex(tx, 32); safe = address(safe);
  const receipt = await c.getTransactionReceipt({hash: tx});
  requireThat(receipt.status === 'success', 'deployment transaction reverted');
  const events = receipt.logs.filter(log => log.address.toLowerCase() === config.kms_auth.toLowerCase()).flatMap(log => {
    try { const event = decodeEventLog({abi, ...log}); return event.eventName === 'AppDeployedViaFactory' ? [event] : []; }
    catch { return []; }
  }).filter(event => address(event.args.deployer) === safe);
  requireThat(events.length === 1, 'expected exactly one AppDeployedViaFactory by this Safe from Base KmsAuth');
  return address(events[0].args.appId);
}
// Topics verified against the bytecode of the pinned deployed implementations in
// deploy/tests/fixtures/base-kms-bytecode.json. Newer upstream PolicyChanged is absent there.
export const appEvents = ['ComposeHashAdded(bytes32)', 'ComposeHashRemoved(bytes32)',
  'DeviceAdded(bytes32)', 'DeviceRemoved(bytes32)', 'AllowAnyDeviceSet(bool)', 'UpgradesDisabled()',
  'RequireTcbUpToDateSet(bool)', 'OwnershipTransferred(address,address)', 'Upgraded(address)'];
// Not AppRegistered: the shared KmsAuth emits it for every tenant's app. Our apps are watched directly.
export const kmsEvents = ['OwnershipTransferred(address,address)', 'Upgraded(address)',
  'GatewayAppIdSet(string)', 'OsImageHashAdded(bytes32)',
  'OsImageHashRemoved(bytes32)', 'KmsAggregatedMrAdded(bytes32)', 'KmsAggregatedMrRemoved(bytes32)',
  'AppImplementationSet(address)', 'KmsInfoSet(bytes)', 'KmsDeviceAdded(bytes32)', 'KmsDeviceRemoved(bytes32)'];
export async function monitor(c, apps, from, to) {
  const targets = [[config.kms_auth, kmsEvents], ...apps.map(app => [address(app), appEvents])];
  const events = [];
  // Public Base RPCs cap log ranges: bounded chunks, no indexer/API key required.
  for (const [target, names] of targets) {
    for (let start = from; start <= to; start += 2000n) {
      const logs = await c.request({method: 'eth_getLogs', params: [{address: target,
        fromBlock: `0x${start.toString(16)}`, toBlock: `0x${(start + 1999n < to ? start + 1999n : to).toString(16)}`,
        topics: [names.map(toEventSelector)]}]});
      events.push(...logs.map(log => ({address: target, event: names.find(name => toEventSelector(name) === log.topics[0]),
        block: String(log.blockNumber), transaction_hash: log.transactionHash, topics: log.topics, data: log.data})));
    }
  }
  return {from_block: String(from), to_block: String(to), events};
}
async function main([command, ...args]) {
  const json = path => JSON.parse(readFileSync(path, 'utf8'));
  switch (command) {
    case 'hash': console.log(composeHash(json(args[0]))); break;
    case 'check-hash': console.log(compareHash(json(args[0]), json(args[1]).compose_hash)); break;
    case 'batch': writeFileSync(args[0], JSON.stringify(batch(...args.slice(1)), null, 2) + '\n'); break;
    case 'cloud-prepare': {
      // Pinned CLI deploy/--prepare-only calls patchCvm({prepareOnly:true}):
      // @phala/cloud src/actions/cvms/patch_cvm.ts, PATCH /cvms/{id}, X-Prepare-Only.
      // Published OpenAPI operationId: update_cvm_endpoint_api_v1_cvms__cvm_id__patch.
      const request = json(args[1]);
      const result = await patchCvm(createClient({timeout: 30000}), {id: args[0], prepareOnly: true,
        docker_compose_file: request.docker_compose_file, pre_launch_script: request.pre_launch_script,
        public_logs: request.public_logs, public_sysinfo: request.public_sysinfo});
      requireThat(result.requiresOnChainHash && result.commitToken, 'prepare returned no onchain commit token');
      const response = {compose_hash: result.composeHash, app_id: result.appId, device_id: result.deviceId,
        token: result.commitToken, kms_info: result.kmsInfo};
      // The bearer token is private working state; never print it or upload it as an artifact.
      writeFileSync(args[2], JSON.stringify(response), {mode: 0o600});
      break;
    }
    case 'verify-provider-event': verifyProviderEvent(json(args[0]), args[1]); break;
    case 'identity': console.log(await identity(await client())); break;
    case 'verify': console.log(JSON.stringify(await verify(await client(), ...args))); break;
    case 'receipt': {
      const receipt = await (await client()).getTransactionReceipt({hash: hex(args[0], 32)});
      requireThat(receipt.status === 'success', 'Safe transaction reverted');
      console.log('confirmed'); break;
    }
    case 'deployment': console.log(await deployment(await client(), ...args)); break;
    case 'monitor': {
      const c = await client();
      const to = await c.getBlockNumber();
      const from = args[0] ? BigInt(args[0]) : to > 45000n ? to - 45000n : 0n;
      const report = await monitor(c, args.slice(1), from, to);
      console.log(JSON.stringify(report));
      if (report.events.length) process.exitCode = 1;
      break;
    }
    default: throw new Error('usage: onchain-kms.mjs hash|check-hash|batch|identity|verify|deployment|monitor ARGS...');
  }
}
if (process.argv[1] === fileURLToPath(import.meta.url)) {
  main(process.argv.slice(2)).catch(error => { console.error(`onchain KMS: ${error.message}`); process.exitCode = 1; });
}
