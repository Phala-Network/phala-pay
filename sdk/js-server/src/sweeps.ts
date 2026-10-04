import {
  encodeFunctionData,
  getAddress,
  isHex,
  keccak256,
  stringToBytes,
  type Address,
  type Hex,
} from "viem";

const FLUSH_ABI = [
  {
    type: "function",
    name: "flush",
    stateMutability: "nonpayable",
    inputs: [
      { name: "treasury", type: "address" },
      { name: "salts", type: "bytes32[]" },
      { name: "token", type: "address" },
    ],
    outputs: [],
  },
] as const;

/** A contract call as a wallet or the Safe Transaction Builder takes it. */
export interface Call {
  to: Address;
  data: Hex;
  /** Native value in wei, a decimal string: `"0"` for a flush. */
  value: string;
}

/**
 * `factory.flush(treasury, salts, token)`, encoded offline from the exported forwarders
 * (`GET /v1/forwarders`): every listed forwarder's whole balance of `token` moves to `treasury`,
 * the only address it can pay. Anyone may send it; the sender pays the gas.
 */
export function flushTransaction(
  factory: string,
  treasury: string,
  salts: readonly Hex[],
  token: string,
): Call {
  if (salts.length === 0) {
    throw new TypeError("flush needs at least one salt");
  }
  if (!salts.every((salt) => isHex(salt) && salt.length === 66)) {
    throw new TypeError("a salt is 32 bytes of hex");
  }
  return {
    to: getAddress(factory),
    data: encodeFunctionData({
      abi: FLUSH_ABI,
      functionName: "flush",
      args: [getAddress(treasury), salts, getAddress(token)],
    }),
    value: "0",
  };
}

/** A forwarder as `GET /v1/forwarders` returns it. */
export interface ForwarderObject {
  chain_id: number;
  factory: string;
  treasury: string;
  salt: string;
}

/**
 * Groups forwarders of one chain, such as `GET /v1/forwarders?sweepable=<token>` lists, into
 * `flushTransaction` calls: one per factory and treasury, of at most `maxSalts` forwarders.
 */
export function flushTransactions(
  forwarders: Iterable<ForwarderObject>,
  token: string,
  maxSalts = 200,
): Call[] {
  const groups = new Map<string, { factory: string; treasury: string; salts: Hex[] }>();
  const chains = new Set<number>();
  for (const forwarder of forwarders) {
    chains.add(forwarder.chain_id);
    const factory = getAddress(forwarder.factory);
    const treasury = getAddress(forwarder.treasury);
    const key = `${factory}:${treasury}`;
    const group = groups.get(key) ?? { factory, treasury, salts: [] };
    if (!isHex(forwarder.salt)) {
      throw new TypeError("a salt is 32 bytes of hex");
    }
    group.salts.push(forwarder.salt);
    groups.set(key, group);
  }
  if (chains.size > 1) {
    throw new TypeError("a batch of flush calls is for one chain");
  }
  const calls: Call[] = [];
  for (const { factory, treasury, salts } of groups.values()) {
    for (let start = 0; start < salts.length; start += maxSalts) {
      calls.push(flushTransaction(factory, treasury, salts.slice(start, start + maxSalts), token));
    }
  }
  return calls;
}

/**
 * The Safe Transaction Builder's batch file, its `BatchFile` type
 * (safe-global/safe-react-apps, `apps/tx-builder/src/typings/models.ts` at commit
 * e8cccfb9a1042fa2954087988bae59c3b8c81780).
 */
export interface BatchFile {
  version: string;
  chainId: string;
  createdAt: number;
  meta: BatchFileMeta;
  transactions: BatchTransaction[];
}

export interface BatchFileMeta {
  txBuilderVersion?: string;
  checksum?: string;
  createdFromSafeAddress?: string;
  createdFromOwnerAddress?: string;
  name: string;
  description?: string;
}

export interface BatchTransaction {
  to: string;
  value: string;
  data?: string;
  contractMethod?: { inputs: unknown[]; name: string; payable: boolean };
  contractInputsValues?: { [key: string]: string };
}

export interface SafeBatchOptions {
  name?: string;
  description?: string;
  /** Milliseconds; now by default. */
  createdAt?: number;
}

/**
 * A Transaction Builder batch file of `calls` for the Safe `safe` on `chainId`, with the app's
 * `meta.checksum`, so it imports without the "modified since it was generated" warning. A Safe
 * owner imports it in Safe{Wallet} > Apps > Transaction Builder; the owners sign and execute it.
 */
export function safeBatch(
  chainId: number,
  safe: string,
  calls: readonly Call[],
  options: SafeBatchOptions = {},
): BatchFile {
  if (calls.length === 0) {
    throw new TypeError("a batch needs at least one call");
  }
  const batch: BatchFile = {
    version: "1.0",
    chainId: String(chainId),
    createdAt: options.createdAt ?? Date.now(),
    meta: {
      name: options.name ?? "Phala Pay sweep",
      description: options.description ?? "",
      createdFromSafeAddress: getAddress(safe),
    },
    transactions: calls.map((call) => {
      if (!/^\d+$/.test(call.value) || !isHex(call.data)) {
        throw new TypeError("a call needs hex data and a decimal value");
      }
      return { to: getAddress(call.to), value: call.value, data: call.data };
    }),
  };
  batch.meta.checksum = batchChecksum(batch);
  return batch;
}

/**
 * The Transaction Builder's `meta.checksum` (`apps/tx-builder/src/lib/checksum.ts`): Keccak-256
 * of its key-sorted serialization with `meta.name` set to `null`, over the file without its
 * checksum, as the app's `validateChecksum` recomputes it.
 */
export function batchChecksum(batch: BatchFile): Hex {
  const meta: Record<string, unknown> = { ...batch.meta, name: null };
  delete meta["checksum"];
  return keccak256(stringToBytes(serialize({ ...batch, meta })));
}

/** The app's `serializeJSONObject`. */
function serialize(value: unknown): string {
  if (Array.isArray(value)) {
    return `[${value.map(serialize).join(",")}]`;
  }
  if (typeof value === "object" && value !== null) {
    const record = value as Record<string, unknown>;
    const keys = Object.keys(record).sort();
    return `{${JSON.stringify(keys)}${keys.map((key) => `${serialize(record[key])},`).join("")}}`;
  }
  return JSON.stringify(value === undefined ? null : value);
}
