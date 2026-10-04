import {
  encodeAbiParameters,
  getAddress,
  getContractAddress,
  keccak256,
  concat,
  type Address,
  type Hex,
} from "viem";

/**
 * The forwarder `factory` deploys for `treasury` and `salt`: OpenZeppelin 5.x
 * `Clones.predictDeterministicAddressWithImmutableArgs` with `abi.encodePacked(treasury)`, so the
 * address commits to the factory, the implementation, the treasury, and the salt.
 */
export function forwarderAddress(
  factory: string,
  implementation: string,
  treasury: string,
  salt: Hex,
): Address {
  const initCode = concat([
    "0x61",
    "0x0041", // runtime length: the 45-byte proxy and the 20 treasury bytes
    "0x3d81600a3d39f3",
    "0x363d3d373d3d3d363d73",
    getAddress(implementation),
    "0x5af43d82803e903d91602b57fd5bf3",
    getAddress(treasury),
  ]);
  return getContractAddress({
    opcode: "CREATE2",
    from: getAddress(factory),
    salt,
    bytecodeHash: keccak256(initCode),
  });
}

/** `keccak256(abi.encode(account, client_reference_id, "quote", quote_id))`: a quote's salt. */
export function quoteSalt(account: string, clientReferenceId: string, quoteId: string): Hex {
  return keccak256(
    encodeAbiParameters(
      [{ type: "string" }, { type: "string" }, { type: "string" }, { type: "string" }],
      [account, clientReferenceId, "quote", quoteId],
    ),
  );
}

/**
 * `keccak256(abi.encode(account, livemode, client_reference_id, "deposit_address", version))`:
 * a deposit address's salt, the same on every chain.
 */
export function depositAddressSalt(
  account: string,
  livemode: boolean,
  clientReferenceId: string,
  version: number | bigint,
): Hex {
  return keccak256(
    encodeAbiParameters(
      [
        { type: "string" },
        { type: "bool" },
        { type: "string" },
        { type: "string" },
        { type: "uint256" },
      ],
      [account, livemode, clientReferenceId, "deposit_address", BigInt(version)],
    ),
  );
}

export interface Forwarder {
  factory: string;
  implementation: string;
}

/** The address of a quote over `treasury`, which you pin yourself (see `verifyQuoteAddress`). */
export function quoteAddress(
  forwarder: Forwarder,
  quote: { client_reference_id: string; id: string },
  treasury: string,
  account: string,
): Address {
  return forwarderAddress(
    forwarder.factory,
    forwarder.implementation,
    treasury,
    quoteSalt(account, quote.client_reference_id, quote.id),
  );
}

/** A deposit address on the network whose treasury is `treasury`, which you pin yourself. */
export function depositAddress(
  forwarder: Forwarder,
  address: { livemode: boolean; client_reference_id: string; version: number },
  treasury: string,
  account: string,
): Address {
  return forwarderAddress(
    forwarder.factory,
    forwarder.implementation,
    treasury,
    depositAddressSalt(account, address.livemode, address.client_reference_id, address.version),
  );
}

/** An address the service returned is not the one your pins derive: show nothing to pay. */
export { AddressMismatchError } from "./errors.js";
import { AddressMismatchError } from "./errors.js";

/**
 * What every address is recomputed from, configured on your server and never read from the
 * service: your account id (`acct_…`), the forwarder `factory` and `implementation` of the
 * attested deployment, and `treasuries`, your own treasury per chain id as you proved it. A
 * compromised service could return another treasury with the address that really derives from
 * it, so the response's `treasury` is never trusted.
 *
 * Live mode requires `treasuries`: without the chain's treasury the check fails closed. In test
 * mode an unpinned chain falls back to the response's treasury with a console warning.
 */
export interface AddressPins extends Forwarder {
  account: string;
  treasuries?: Readonly<Record<number, string>> | undefined;
}

/**
 * The treasury an address on `chainId` must pay: the pinned one, which the response must name.
 */
function pinnedTreasury(
  pins: AddressPins,
  livemode: boolean,
  chainId: number,
  treasury: string,
  where: string,
): string {
  const pinned = pins.treasuries?.[chainId];
  if (pinned === undefined) {
    if (livemode) {
      throw new AddressMismatchError(`${where}: live mode requires your pinned treasury of chain ${chainId}`);
    }
    console.warn(
      `${where}: no pinned treasury of chain ${chainId}; the service's ${treasury} is trusted (test mode only)`,
    );
    return treasury;
  }
  if (!sameAddress(pinned, treasury)) {
    throw new AddressMismatchError(`${where} pays a treasury that is not the pinned one`);
  }
  return pinned;
}

function requirePins(pins: AddressPins, where: string): void {
  for (const field of ["account", "factory", "implementation"] as const) {
    if (typeof pins[field] !== "string" || pins[field] === "") {
      throw new AddressMismatchError(`${where}: the pinned ${field} is missing`);
    }
  }
}

function sameAddress(a: string, b: string): boolean {
  return a.toLowerCase() === b.toLowerCase();
}

/**
 * Recomputes an open quote's address from your pins and returns it, the `expectedAddress` to
 * pass to `<Checkout>`. Throws `AddressMismatchError` unless the quote names your pinned
 * treasury of its chain and its address derives from it; in live mode also when the chain has no
 * pinned treasury.
 */
export function verifyQuoteAddress(
  pins: AddressPins,
  quote: {
    id: string;
    livemode: boolean;
    chain_id: number;
    treasury: string;
    address: string;
    client_reference_id: string;
  },
): Address {
  const where = `quote ${quote.id}`;
  requirePins(pins, where);
  const treasury = pinnedTreasury(pins, quote.livemode, quote.chain_id, quote.treasury, where);
  const derived = quoteAddress(pins, quote, treasury, pins.account);
  if (!sameAddress(derived, quote.address)) {
    throw new AddressMismatchError(`${where} has an address the account cannot derive`);
  }
  return derived;
}

/**
 * Recomputes every network of an active deposit address from your pins and returns the address,
 * the same on every network. Throws `AddressMismatchError` as `verifyQuoteAddress` does, for any
 * network.
 */
export function verifyDepositAddress(
  pins: AddressPins,
  address: {
    id: string;
    livemode: boolean;
    client_reference_id: string;
    version: number;
    networks: readonly { chain_id: number; treasury: string; address: string }[];
  },
): Address {
  requirePins(pins, `deposit address ${address.id}`);
  let first: Address | undefined;
  for (const network of address.networks) {
    const where = `deposit address ${address.id} on chain ${network.chain_id}`;
    const treasury = pinnedTreasury(pins, address.livemode, network.chain_id, network.treasury, where);
    const derived = depositAddress(pins, address, treasury, pins.account);
    if (!sameAddress(derived, network.address)) {
      throw new AddressMismatchError(`${where} is not one the account can derive`);
    }
    first ??= derived;
  }
  if (first === undefined) {
    throw new AddressMismatchError(`deposit address ${address.id} has no network`);
  }
  return first;
}
