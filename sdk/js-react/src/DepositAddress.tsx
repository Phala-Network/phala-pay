"use client";

import { useEffect, useId, useRef, useState } from "react";
import { formatUnits, type WalletClient } from "viem";
import {
  WalletError,
  formatUnitsGrouped,
  networkName,
  payDepositAddressWithWallet,
  paymentOptions,
  samePaymentOption,
  transactionUrl,
  type DepositAddressAsset,
  type DepositAddressNetwork,
  type EthereumProvider,
  type PaymentOption,
} from "@phala/pay";
import {
  retrieveDepositAddress,
  type ClientDepositAddress,
  type DepositAddressPayment,
} from "@phala/pay";
import { CheckoutError, pollDelay } from "@phala/pay";
import { formatWait } from "@phala/pay";
import { depositAddressTransfer, type DepositAddressDetails } from "@phala/pay";
import { rootProps, type Appearance } from "./appearance.js";
import { Field } from "./Field.js";
import { ExternalGlyph, ToneGlyph } from "./Glyphs.js";
import { AssetIcon, NetworkIcon } from "./Icons.js";
import { depositLimits } from "./limits.js";
import { PaymentOptionFields } from "./PaymentOptionPicker.js";
import { QrCode } from "./QrCode.js";
import { useWallets, type WalletChoice } from "./wallets.js";

export interface DepositAddressProps {
  /** `address` and `networks` from your backend's `POST /v1/deposit_addresses`. */
  depositAddress: DepositAddressDetails;
  /** The network shown first; the first of `networks` by default. */
  chainId?: number | undefined;
  /** The token shown first, for example `pha`; the network's first token by default. */
  asset?: string | undefined;
  /**
   * The `client_secret` of the same response. With `apiBase`, the component follows the
   * address's payments and shows each one within about a block of arriving: received with its
   * confirmations, then credited, and states each network's typical credit time. Display only;
   * credit from your `deposit.credited` webhook.
   */
  clientSecret?: string | undefined;
  /** The service origin, for example `https://pay.example.com`; needed with `clientSecret`. */
  apiBase?: string | undefined;
  /** Milliseconds between payment reads; default 3000. */
  pollInterval?: number | undefined;
  /** Called after the first successful read and once per public-view content change.
   * Unlike Checkout's `onChange`, this does not report loading.
   * Display only; credit from your `deposit.credited` webhook. */
  onChange?: (state: ClientDepositAddress) => void;
  /**
   * The amount to pay, in the smallest unit of the token at `chainId` and `asset`, which then stay
   * fixed. The component states the amount once, offers to pay it from a browser wallet (or
   * `walletClient`), and, as a secondary way, a QR code that requests exactly this amount (EIP-681
   * `uint256`) and the address to copy. With `clientSecret` and `apiBase`, it follows that payment:
   * the one the wallet sent, by its transaction hash, else the newest one that arrived after the
   * address was first read, and reports it on one status line.
   */
  amountAtomic?: bigint | undefined;
  /** With `amountAtomic`: the wallet button's label; default "Pay with <wallet name>". */
  buttonText?: string | undefined;
  /** With `amountAtomic`: as `<Checkout walletClient>`. */
  walletClient?: WalletClient | undefined;
  /** With `amountAtomic`: as `<Checkout onWalletError>`. */
  onWalletError?: ((error: WalletError, wallet: WalletClient | EthereumProvider) => void) | undefined;
  /**
   * With `amountAtomic`: called with the payment the component follows whenever its status or
   * confirmations change, and with `null` before there is one. Display only.
   */
  onPaymentChange?: ((payment: DepositAddressPayment | null) => void) | undefined;
  /**
   * With `amountAtomic`: called with the transaction hash and chain once the wallet has sent the
   * payment. Keep them to resume following it after a reload, with `txHash`.
   */
  onSent?: ((txHash: string, chainId: number) => void) | undefined;
  /**
   * With `amountAtomic`: a payment already sent on `chainId` (from `onSent`), for example before a
   * reload. The component follows it, by this hash, instead of offering to pay.
   */
  txHash?: string | undefined;
  /**
   * With `amountAtomic`: called once when the payment it follows is credited. Its `amount_atomic`
   * is what arrived. Fulfil from your `deposit.credited` webhook, not here.
   */
  onSuccess?: ((payment: DepositAddressPayment) => void) | undefined;
  appearance?: Appearance;
  className?: string;
}

/**
 * A customer's persistent deposit address: one address for every supported token on every
 * supported network. The payer picks a token and network; the component shows a QR code of that
 * EIP-681 transfer request, per-transfer limits, and the address and token contract to copy.
 * With `amountAtomic`, it asks for that amount of one token instead, paid from a wallet first.
 * Eligible transfers are credited at the market rate; follow them from your backend's
 * `deposit.credited` webhook.
 */
export function DepositAddress({ appearance, className, ...props }: DepositAddressProps) {
  return (
    <div {...rootProps(appearance, className)}>
      <DepositAddressContent {...props} />
    </div>
  );
}

/** The deposit address without its `.pp-root`, for a component that is itself inside one. */
export function DepositAddressContent({
  depositAddress,
  chainId,
  asset,
  clientSecret,
  apiBase,
  pollInterval,
  onChange,
  amountAtomic,
  ...payment
}: Omit<DepositAddressProps, "appearance" | "className">) {
  const { networks } = depositAddress;
  const { view, reconnecting } = useClientView(clientSecret, apiBase, pollInterval ?? 3000, onChange);
  const payments = view?.payments ?? [];
  const options = paymentOptions(networks);
  const [selected, setSelected] = useState<PaymentOption>(() => ({
    chain_id: chainId ?? networks[0]?.chain_id ?? 0,
    asset: asset ?? "",
  }));
  if (amountAtomic !== undefined) {
    const fixed = options.find(
      (each) => each.chain_id === (chainId ?? networks[0]?.chain_id) && (asset === undefined || each.asset === asset),
    );
    if (fixed === undefined) {
      throw new TypeError("the deposit address does not take the token on the network to pay");
    }
    return (
      <DepositPayment
        network={fixed.network}
        token={fixed.token}
        amountAtomic={amountAtomic}
        view={view}
        reconnecting={reconnecting}
        clientSecret={clientSecret}
        apiBase={apiBase}
        {...payment}
      />
    );
  }
  // The chosen token on the chosen network, else that network's first token, else the first.
  const option =
    options.find((each) => samePaymentOption(each, selected)) ??
    options.find((each) => each.chain_id === selected.chain_id) ??
    options[0];
  if (option === undefined) {
    throw new TypeError("the deposit address takes no token on any network");
  }
  const { network, token } = option;
  const { token: contract, to } = depositAddressTransfer(network, token);
  const symbol = token.asset.toUpperCase();
  const name = networkName(network.chain_id);
  const currentToken = view?.networks
    .find((each) => each.chain_id === network.chain_id)?.assets
    ?.find((each) => each.asset === token.asset) ?? token;
  const amount = depositLimits(currentToken) ?? symbol;
  return (
    <>
      <p className="pp-intro">
        {depositAddress.address === null
          ? "Reusable deposit address; varies by network"
          : "One reusable address for supported tokens and networks"}
      </p>
      <PaymentOptionFields
        options={options}
        value={option}
        onChange={setSelected}
      />
      <div className="pp-qr">
        <QrCode value={token.payment_uri} label={`Deposit address for ${symbol} on ${name}`} />
      </div>
      <dl className="pp-fields">
        <Field label="Network" value={`${name} (chain ID ${network.chain_id})`} />
        <Field label={`Token (${symbol}) contract`} value={contract} copy />
        <Field label="Deposit address" value={to} copy />
      </dl>
      <p className="pp-message" role="status">{reconnecting ? "Reconnecting…" : ""}</p>
      {payments.length > 0 && (
        <ul className="pp-payments" aria-live="polite" aria-label="Payments">
          {payments.map((payment) => (
            <li
              className="pp-payments__item"
              key={`${payment.chain_id}:${payment.tx_hash}:${payment.created}`}
              data-tone={paymentTone(payment)}
            >
              <span className="pp-payments__dot" aria-hidden="true" />
              {paymentMessage(payment)}
            </li>
          ))}
        </ul>
      )}
      <p className="pp-message">
        Send {amount} on {name}. Unsupported tokens or networks are not credited.{" "}
        {creditMessage(networks.map((each) => each.chain_id), view)}
      </p>
    </>
  );
}

type PaymentStep =
  | { kind: "idle" }
  | { kind: "pending"; wallet: string }
  | { kind: "failed"; message: string }
  | { kind: "sent"; hash: string };

/** One amount of one token to the address: wallet first, then a QR code and the address. */
function DepositPayment({
  network,
  token,
  amountAtomic,
  view,
  reconnecting,
  clientSecret,
  apiBase,
  buttonText,
  walletClient,
  onWalletError,
  onPaymentChange,
  onSuccess,
  onSent,
  txHash,
}: Pick<
  DepositAddressProps,
  | "clientSecret"
  | "apiBase"
  | "buttonText"
  | "walletClient"
  | "onWalletError"
  | "onPaymentChange"
  | "onSuccess"
  | "onSent"
  | "txHash"
> & {
  network: DepositAddressNetwork;
  token: DepositAddressAsset;
  amountAtomic: bigint;
  view: ClientDepositAddress | null;
  reconnecting: boolean;
}) {
  const wallets = useWallets(walletClient);
  const [step, setStep] = useState<PaymentStep>(() =>
    txHash !== undefined && /^0x[0-9a-fA-F]{64}$/.test(txHash) ? { kind: "sent", hash: txHash } : { kind: "idle" },
  );
  const [open, setOpen] = useState(false);
  const manualId = useId();
  const { to } = depositAddressTransfer(network, token);
  const symbol = token.asset.toUpperCase();
  const name = networkName(network.chain_id);
  const amount = `${formatUnitsGrouped(amountAtomic, token.decimals)} ${symbol}`;

  // The payments already there when the address was first read are not this one.
  const [earlier, setEarlier] = useState<ReadonlySet<string> | null>(null);
  if (view !== null && earlier === null) {
    setEarlier(new Set(view.payments.map(paymentKey)));
  }
  const sent = step.kind === "sent" ? step.hash.toLowerCase() : null;
  const tracked = view?.payments.find((each) =>
    sent === null
      ? earlier !== null && !earlier.has(paymentKey(each))
      : each.chain_id === network.chain_id && each.tx_hash.toLowerCase() === sent,
  );

  const callbacks = useRef({ onPaymentChange, onSuccess });
  const current = useRef(tracked);
  useEffect(() => {
    callbacks.current = { onPaymentChange, onSuccess };
    current.current = tracked;
  });
  const status = tracked === undefined ? "" : `${paymentKey(tracked)} ${tracked.status} ${tracked.confirmations ?? ""}`;
  const credited = useRef(new Set<string>());
  useEffect(() => {
    const payment = current.current ?? null;
    callbacks.current.onPaymentChange?.(payment);
    if (payment?.status === "credited" && !credited.current.has(paymentKey(payment))) {
      credited.current.add(paymentKey(payment));
      callbacks.current.onSuccess?.(payment);
    }
  }, [status]);

  const pay = async ({ name: walletName, wallet }: WalletChoice) => {
    setStep({ kind: "pending", wallet: walletName });
    try {
      const hash = await payDepositAddressWithWallet(wallet, {
        network,
        asset: token,
        amountAtomic,
        clientSecret,
        apiBase,
      });
      setStep({ kind: "sent", hash });
      onSent?.(hash, network.chain_id);
    } catch (error) {
      setStep({ kind: "failed", message: error instanceof WalletError ? error.message : "The payment could not be sent" });
      if (error instanceof WalletError) {
        onWalletError?.(error, wallet);
      }
    }
  };

  const tone = step.kind === "failed" ? "danger" : tracked === undefined ? "neutral" : paymentTone(tracked);
  const message =
    step.kind === "failed"
      ? step.message
      : step.kind === "pending"
        ? `Confirm the payment in ${step.wallet}…`
        : tracked !== undefined
          ? trackedMessage(tracked, network, token, amountAtomic, view)
          : sent !== null
            ? "Sent, waiting for it on chain…"
            : "";
  const url = step.kind === "sent" ? transactionUrl(network.chain_id, step.hash) : undefined;
  // Once a payment is on its way, the ways to pay give way to its status.
  const underway = sent !== null || tracked !== undefined;
  const manual = (
    <>
      <div className="pp-qr">
        <QrCode value={`${token.payment_uri}&uint256=${amountAtomic}`} label={`Payment request for ${amount} on ${name}`} />
        <p className="pp-qr__caption">Send exactly this amount in {symbol} on {name}, in one transfer.</p>
      </div>
      <dl className="pp-fields">
        <Field label="Deposit address" value={to} copy />
      </dl>
    </>
  );
  return (
    <>
      <div className="pp-summary">
        <p className="pp-summary__amount">
          <AssetIcon asset={token.asset} size={20} />
          {amount}
        </p>
        <p className="pp-summary__meta">
          <span className="pp-summary__network">
            <NetworkIcon chainId={network.chain_id} size={16} />
            {name}
          </span>
        </p>
      </div>
      {!underway && wallets.length > 0 && (
        <div className="pp-wallets">
          {wallets.map((choice) => (
            <button
              key={choice.id}
              type="button"
              className="pp-button"
              disabled={step.kind === "pending"}
              onClick={() => void pay(choice)}
            >
              {choice.icon !== "" && <img className="pp-button__icon" src={choice.icon} alt="" />}
              {buttonText === undefined ? (
                <span>Pay with {choice.name}</span>
              ) : (
                <>
                  <span>{buttonText}</span>
                  {walletClient === undefined && (
                    <>
                      {" "}
                      <span className="pp-button__detail">{choice.name}</span>
                    </>
                  )}
                </>
              )}
            </button>
          ))}
        </div>
      )}
      <p className="pp-message pp-deposit__status" data-tone={tone} role="status">
        {(message !== "" || reconnecting) && (
          <>
            {/* One plain line while in progress; a mark only for its outcome. */}
            {message !== "" && tone !== "neutral" && <ToneGlyph tone={tone} />}
            <span>
              {message}
              {reconnecting && (message === "" ? "Reconnecting…" : " (reconnecting…)")}
              {url !== undefined && (
                <>
                  {" "}
                  <a className="pp-deposit__tx" href={url} target="_blank" rel="noreferrer">
                    View transaction
                    <ExternalGlyph />
                  </a>
                </>
              )}
            </span>
          </>
        )}
      </p>
      {underway ? null : wallets.length === 0 ? (
        <div className="pp-deposit__manual">{manual}</div>
      ) : (
        <>
          <p className="pp-deposit__more">
            <button
              type="button"
              className="pp-text-button"
              aria-expanded={open}
              aria-controls={manualId}
              onClick={() => setOpen(!open)}
            >
              Pay from an exchange or another wallet
            </button>
          </p>
          <div className="pp-deposit__manual" id={manualId} hidden={!open}>
            {manual}
          </div>
        </>
      )}
    </>
  );
}

/**
 * The status of the payment asked for, as the checkout states a quote's; a payment of another
 * amount, token, or network is named in full, as it is what arrived.
 */
function trackedMessage(
  payment: DepositAddressPayment,
  network: DepositAddressNetwork,
  token: DepositAddressAsset,
  amountAtomic: bigint,
  view: ClientDepositAddress | null,
): string {
  if (payment.chain_id !== network.chain_id || payment.asset !== token.asset || BigInt(payment.amount_atomic) !== amountAtomic) {
    return paymentMessage(payment);
  }
  switch (payment.status) {
    case "seen": {
      const seconds = view?.networks.find((each) => each.chain_id === payment.chain_id)?.typical_credit_seconds;
      const received =
        payment.confirmations === null
          ? "Received"
          : `Received, ${payment.confirmations} confirmation${payment.confirmations === 1 ? "" : "s"}`;
      return seconds === undefined ? `${received}, crediting shortly` : `${received}. Crediting in ${formatWait(seconds)}`;
    }
    case "confirming":
      return "Confirmed on chain, crediting…";
    case "credited":
      return "Payment credited";
    case "rejected":
      return "This payment cannot be credited. Contact support with your transaction.";
    case "reversed":
      return "Payment reversed: a chain reorganization replaced it and its credit was removed.";
  }
}

function paymentKey(payment: DepositAddressPayment): string {
  return `${payment.chain_id}:${payment.tx_hash.toLowerCase()}`;
}

/** The address's public view, read every `interval` while mounted; `null` without a secret. */
function useClientView(
  clientSecret: string | undefined,
  apiBase: string | undefined,
  interval: number,
  onChange: DepositAddressProps["onChange"],
): { view: ClientDepositAddress | null; reconnecting: boolean } {
  const callback = useRef(onChange);
  useEffect(() => {
    callback.current = onChange;
  });
  const key = clientSecret === undefined || apiBase === undefined ? "" : `${apiBase} ${clientSecret}`;
  const lastView = useRef<{ key: string; serialized: string } | null>(null);
  const [current, setCurrent] = useState<{ key: string; view: ClientDepositAddress | null; reconnecting: boolean }>({
    key: "",
    view: null,
    reconnecting: false,
  });
  useEffect(() => {
    if (clientSecret === undefined || apiBase === undefined) {
      return;
    }
    const controller = new AbortController();
    let stopped = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let failures = 0;
    let inFlight = false;
    let finished = false;
    let lastChanged = Date.now();
    const visibilityDocument = typeof document === "undefined" ? undefined : document;
    const hidden = () => visibilityDocument?.visibilityState === "hidden";
    const load = async () => {
      if (controller.signal.aborted || finished || inFlight || hidden()) {
        return;
      }
      clearTimeout(timer);
      inFlight = true;
      let failure: CheckoutError | null = null;
      let changedView: ClientDepositAddress | undefined;
      try {
        const view = await retrieveDepositAddress({ clientSecret, apiBase, signal: controller.signal });
        failures = 0;
        if (!stopped) {
          const serialized = JSON.stringify(view);
          const previous = lastView.current;
          if (previous === null || previous.key !== key || previous.serialized !== serialized) {
            lastView.current = { key, serialized };
            lastChanged = Date.now();
            changedView = view;
          }
          setCurrent({ key, view, reconnecting: false });
        }
      } catch (error) {
        failures += 1;
        // A secret that is not valid will not become valid: stop, keep showing the address.
        if (error instanceof CheckoutError && error.code === "invalid_client_secret") {
          finished = true;
          if (!stopped) setCurrent((previous) => ({ ...previous, reconnecting: false }));
          return;
        }
        failure = error instanceof CheckoutError ? error : null;
        if (!stopped && (failure === null || failure.code === "service_unavailable" || failure.code === "network_error")) {
          setCurrent((previous) => ({
            key, view: previous.key === key ? previous.view : null, reconnecting: true,
          }));
        }
      } finally {
        inFlight = false;
      }
      if (!stopped && !hidden()) {
        const nextInterval = Date.now() - lastChanged >= 600_000 ? Math.max(interval, 15_000) : interval;
        timer = setTimeout(() => void load(), pollDelay(nextInterval, failures, failure));
      }
      // Notify every observed change, even when React batches multiple polling updates.
      // Consumer callbacks run outside the request catch so they cannot become network errors.
      if (!stopped && changedView !== undefined) {
        try {
          callback.current?.(changedView);
        } catch (error) {
          queueMicrotask(() => { throw error; });
        }
      }
    };
    const visibilityChanged = () => {
      clearTimeout(timer);
      if (visibilityDocument?.visibilityState !== "hidden") {
        lastChanged = Date.now();
        void load();
      }
    };
    visibilityDocument?.addEventListener("visibilitychange", visibilityChanged);
    void load();
    return () => {
      stopped = true;
      controller.abort();
      clearTimeout(timer);
      visibilityDocument?.removeEventListener("visibilitychange", visibilityChanged);
    };
  }, [key, clientSecret, apiBase, interval]);
  // Another address starts without the previous one's view.
  return current.key === key && key !== "" ? current : { view: null, reconnecting: false };
}

/**
 * Seconds within which the service's discovery scan finds a transfer sent without a
 * transaction-hash hint, as this component never submits one: the server's `FAST_INTERVAL`
 * (crates/topup/src/scanner/mod.rs); change both together. The view's `typical_credit_seconds`
 * starts at confirmation (docs/integration.md, "Transaction-hash hints").
 */
const DISCOVERY_SECONDS = 300;

/**
 * When a payment on each of `chainIds` is credited: the discovery scan, then `view`'s typical
 * credit time, for example "usually within about 6 minutes on Sepolia and about 5 minutes on
 * Base Sepolia". Before the view is loaded it names no time.
 */
function creditMessage(chainIds: number[], view: ClientDepositAddress | null): string {
  const names = new Map<string, string[]>();
  for (const network of view?.networks ?? []) {
    if (chainIds.includes(network.chain_id)) {
      const wait = formatWait(DISCOVERY_SECONDS + network.typical_credit_seconds);
      names.set(wait, [...(names.get(wait) ?? []), networkName(network.chain_id)]);
    }
  }
  if (names.size === 0) {
    return "Eligible transfers are credited at the market rate after confirmation.";
  }
  const list = new Intl.ListFormat("en", { type: "conjunction" });
  const [only, ...others] = names.keys();
  const within =
    only !== undefined && others.length === 0
      ? only
      : list.format([...names].map(([wait, on]) => `${wait} on ${list.format(on)}`));
  return `Eligible transfers are credited at the market rate, usually within ${within}.`;
}

function paymentTone({ status }: DepositAddressPayment): "neutral" | "success" | "danger" {
  return status === "credited" ? "success" : status === "rejected" || status === "reversed" ? "danger" : "neutral";
}

function paymentMessage(payment: DepositAddressPayment): string {
  const amount =
    payment.asset === null || payment.decimals === null
      ? "A payment"
      : `${formatUnits(BigInt(payment.amount_atomic), payment.decimals)} ${payment.asset.toUpperCase()}`;
  const on = `on ${networkName(payment.chain_id)}`;
  switch (payment.status) {
    case "seen":
      return payment.confirmations === null
        ? `${amount} received ${on}, crediting shortly`
        : `${amount} received ${on}, ${payment.confirmations} confirmation${payment.confirmations === 1 ? "" : "s"}`;
    case "confirming":
      return `${amount} confirmed ${on}, crediting…`;
    case "credited":
      return `${amount} ${on} credited`;
    case "rejected":
      return `${amount} ${on} cannot be credited. Contact support with your transaction.`;
    case "reversed":
      return `${amount} ${on} was reversed: a chain reorganization replaced it and its credit was removed.`;
  }
}
