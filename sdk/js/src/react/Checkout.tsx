"use client";

import { useEffect, useId, useRef, useState, type KeyboardEvent } from "react";
import type { Hash, WalletClient } from "viem";
import type { CheckoutErrorCode, CheckoutState, CheckoutStatus } from "../checkout.js";
import { networkName, transactionUrl } from "../chains.js";
import {
  formatAmount,
  formatCountdown,
  formatMinorAmount,
  formatTokenAmount,
  formatWait,
  tokenAmount,
} from "../format.js";
import { quoteTransfer } from "../payment.js";
import type { ClientQuote } from "../quote.js";
import {
  WalletError,
  payWithWallet,
  watchWallets,
  type EthereumProvider,
  type Wallet,
} from "../wallet.js";
import type { Appearance } from "./appearance.js";
import { AssetIcon, NetworkIcon } from "./Icons.js";
import { Field } from "./Field.js";
import { QrCode } from "./QrCode.js";
import { useCheckout } from "./useCheckout.js";

export interface CheckoutProps {
  /** The quote's `client_secret`, from your backend's `POST /v1/quotes`. */
  clientSecret: string;
  /**
   * The quote's `address` as your backend's SDK recomputed it from the pinned forwarder
   * (`PhalaPay(forwarder=…)` in Python). Required: when the service's quote names another
   * address, the checkout fails closed and shows nothing to pay.
   */
  expectedAddress: string;
  /** The service origin, for example `https://topup.example.com`. */
  apiBase: string;
  /** Called once when the payment is credited, with the quote: its `amount_credited` is what was
   * credited, which differs from `amount` for a payment of another amount or a late one. Fulfil
   * from the `deposit.credited` webhook, not here. */
  onSuccess?: (quote: ClientQuote) => void;
  /** Called once when the quote expires or is canceled without a payment. */
  onExpire?: (quote: ClientQuote) => void;
  /** Called whenever the checkout's status changes, like Stripe Elements' `onChange`: for example,
   * to hide your own "new payment" control while a payment is `seen` or `confirming`. */
  onChange?: (state: CheckoutState) => void;
  /** Called when the wallet tab could not send the payment, with the `WalletError` and the wallet
   * it tried (the chosen browser wallet's provider, or `walletClient`): for example on
   * `insufficient_balance` (nothing was sent), to offer your own way to fund that wallet. The
   * checkout shows the error's message itself. */
  onWalletError?: (error: WalletError, wallet: WalletClient | EthereumProvider) => void;
  appearance?: Appearance;
  /** Milliseconds between status reads; default 3000. */
  pollInterval?: number;
  className?: string;
  /** The wallet button's label; default "Pay with crypto". */
  buttonText?: string;
  /** Your page's connected wallet, for example wagmi's `useWalletClient().data`. When set, the
   * wallet tab pays with it and its account instead of listing the browser's wallets. */
  walletClient?: WalletClient | undefined;
}

type Method = "wallet" | "qr" | "manual";

const METHODS: { id: Method; label: string }[] = [
  { id: "wallet", label: "Wallet" },
  { id: "qr", label: "QR code" },
  { id: "manual", label: "Manual" },
];

/** A checkout for one quote: pay from a browser wallet, by QR code, or manually, with live status. */
export function Checkout({
  clientSecret,
  expectedAddress,
  apiBase,
  onSuccess,
  onExpire,
  onChange,
  onWalletError,
  appearance,
  pollInterval,
  className,
  buttonText = "Pay with crypto",
  walletClient,
}: CheckoutProps) {
  const { status, quote, error, reconnecting, refresh } = useCheckout({
    clientSecret,
    expectedAddress,
    apiBase,
    ...(pollInterval === undefined ? {} : { pollInterval }),
  });
  const [txHash, setTxHash] = useState<Hash | null>(null);
  const now = useNow(status === "waiting");

  const callbacks = useRef({ onSuccess, onExpire, onChange });
  const state = useRef<CheckoutState>({ status, quote, error });
  useEffect(() => {
    callbacks.current = { onSuccess, onExpire, onChange };
    state.current = { status, quote, error };
  });
  // Once per status change, not on every poll.
  useEffect(() => {
    callbacks.current.onChange?.(state.current);
  }, [status]);
  const notified = useRef(new Set<string>());
  useEffect(() => {
    if (quote === null) {
      return;
    }
    if (status === "credited") {
      const key = `${quote.id}:success`;
      if (!notified.current.has(key)) {
        notified.current.add(key);
        callbacks.current.onSuccess?.(quote);
      }
    } else if (status === "expired" || status === "canceled") {
      const key = `${quote.id}:expire`;
      if (!notified.current.has(key)) {
        notified.current.add(key);
        callbacks.current.onExpire?.(quote);
      }
    }
  }, [status, quote]);

  return (
    <div
      className={className === undefined ? "pp-root" : `pp-root ${className}`}
      data-theme={appearance?.theme ?? "light"}
    >
      {quote !== null && (
        <>
          <p className="pp-amount">
            <AssetIcon asset={quote.asset} size={20} />{" "}
            {formatTokenAmount(quote)} {quote.asset.toUpperCase()}
          </p>
          <p className="pp-subtitle">
            {formatAmount(quote)} top-up · <NetworkIcon chainId={quote.chain_id} size={16} />{" "}
            {networkName(quote.chain_id)}
            {!quote.livemode && <span className="pp-badge"> · Test mode</span>}
          </p>
        </>
      )}
      <StatusLine status={status} quote={quote} now={now} error={error} reconnecting={reconnecting ?? false} />
      {txHash !== null && quote !== null && <Transaction hash={txHash} chainId={quote.chain_id} />}
      {status === "waiting" && quote !== null && (
        <PaymentOptions
          quote={quote}
          buttonText={buttonText}
          walletClient={walletClient}
          onSent={(hash) => {
            setTxHash(hash);
            refresh();
          }}
          onWalletError={onWalletError}
        />
      )}
    </div>
  );
}

function StatusLine({
  status,
  quote,
  now,
  error,
  reconnecting,
}: {
  status: CheckoutStatus;
  quote: ClientQuote | null;
  now: number;
  error: CheckoutState["error"];
  reconnecting: boolean;
}) {
  const tone =
    status === "credited"
      ? "success"
      : ["rejected", "reversed", "expired", "canceled", "error"].includes(status)
        ? "danger"
        : "neutral";
  return (
    <div className="pp-status" data-tone={tone}>
      <span role="status" aria-live="polite">
        {statusMessage(status, quote, error?.code)}
        {(reconnecting || error !== null) && status !== "error" ? " (reconnecting…)" : ""}
      </span>
      {status === "waiting" && quote !== null && (
        <span className="pp-countdown" aria-label="Time left to pay">
          {formatCountdown(quote.expires_at, now)}
        </span>
      )}
    </div>
  );
}

function statusMessage(
  status: CheckoutStatus,
  quote: ClientQuote | null,
  code: CheckoutErrorCode | undefined,
): string {
  switch (status) {
    case "loading":
      return "Loading payment details…";
    case "waiting":
      return "Waiting for your payment";
    case "seen":
      return seenMessage(quote);
    case "confirming":
      return "Confirmed on chain, crediting…";
    case "credited":
      return creditedMessage(quote);
    case "rejected":
      return "This payment cannot be credited. Contact support with your transaction.";
    case "reversed":
      return "Payment reversed: a chain reorganization replaced it and its credit was removed. Check your wallet, then start a new top-up.";
    case "expired":
      return "Quote expired. Do not send funds; start a new top-up.";
    case "canceled":
      return "Quote canceled. Do not send funds.";
    case "error":
      return code === "address_mismatch"
        ? "This payment address could not be verified. Do not send funds; contact support."
        : "This payment link is not valid. Start a new top-up.";
  }
}

function seenMessage(quote: ClientQuote | null): string {
  if (quote === null) {
    return "Received, crediting after confirmation";
  }
  const wait = formatWait(quote.typical_credit_seconds);
  return quote.confirmations === null
    ? `Received, crediting in ${wait}`
    : `Received, ${quote.confirmations} confirmation${quote.confirmations === 1 ? "" : "s"}. Crediting in ${wait}`;
}

/** What was credited and, when the payment was valued at the market price, the quote it missed. */
function creditedMessage(quote: ClientQuote | null): string {
  if (quote?.amount_credited == null) {
    return "Payment credited";
  }
  const credited = formatMinorAmount(quote, quote.amount_credited);
  if (quote.amount_credited < quote.amount) {
    return `Payment credited: ${credited} of ${formatAmount(quote)}`;
  }
  if (quote.amount_credited > quote.amount) {
    return `Payment credited: ${credited} (${formatAmount(quote)} quoted)`;
  }
  return `Payment credited: ${credited}`;
}

function Transaction({ hash, chainId }: { hash: Hash; chainId: number }) {
  const url = transactionUrl(chainId, hash);
  return (
    <p className="pp-tx">
      Transaction sent:{" "}
      {url === undefined ? (
        <span className="pp-value">{hash}</span>
      ) : (
        <a className="pp-value" href={url} target="_blank" rel="noreferrer">
          {hash}
        </a>
      )}
    </p>
  );
}

function PaymentOptions({
  quote,
  buttonText,
  walletClient,
  onSent,
  onWalletError,
}: {
  quote: ClientQuote;
  buttonText: string;
  walletClient: WalletClient | undefined;
  onSent: (hash: Hash) => void;
  onWalletError: CheckoutProps["onWalletError"];
}) {
  const [selectedMethod, setMethod] = useState<Method | null>(null);
  const [discovered, setDiscovered] = useState<Wallet[]>([]);
  useEffect(
    () => (walletClient === undefined ? watchWallets(setDiscovered) : undefined),
    [walletClient],
  );
  const method = selectedMethod ?? (walletClient !== undefined || discovered.length > 0 ? "wallet" : "qr");
  const id = useId();
  const tabs = useRef<(HTMLButtonElement | null)[]>([]);
  const amount = `${formatTokenAmount(quote)} ${quote.asset.toUpperCase()}`;

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const index = METHODS.findIndex((m) => m.id === method);
    const next =
      event.key === "ArrowRight"
        ? (index + 1) % METHODS.length
        : event.key === "ArrowLeft"
          ? (index - 1 + METHODS.length) % METHODS.length
          : event.key === "Home"
            ? 0
            : event.key === "End"
              ? METHODS.length - 1
              : null;
    const target = next === null ? undefined : METHODS[next];
    if (next !== null && target !== undefined) {
      event.preventDefault();
      setMethod(target.id);
      tabs.current[next]?.focus();
    }
  };

  return (
    <>
      <p className="pp-notice">
        Send <strong>exactly {amount}</strong> on {networkName(quote.chain_id)} in one transfer
        before the timer ends. Otherwise it’s credited at the market rate.
      </p>
      <div className="pp-tabs" role="tablist" aria-label="Payment method" onKeyDown={onKeyDown}>
        {METHODS.map((m, index) => (
          <button
            key={m.id}
            ref={(element) => {
              tabs.current[index] = element;
            }}
            type="button"
            role="tab"
            className="pp-tab"
            id={`${id}-tab-${m.id}`}
            aria-label={m.id === "wallet" ? "Browser wallet" : m.id === "manual" ? "Manual transfer" : m.label}
            aria-selected={method === m.id}
            aria-controls={`${id}-panel-${m.id}`}
            tabIndex={method === m.id ? 0 : -1}
            onClick={() => setMethod(m.id)}
          >
            {m.label}
          </button>
        ))}
      </div>
      <div
        role="tabpanel"
        id={`${id}-panel-${method}`}
        aria-labelledby={`${id}-tab-${method}`}
        tabIndex={0}
      >
        {method === "wallet" && (
          <WalletPanel
            quote={quote}
            buttonText={buttonText}
            walletClient={walletClient}
            discovered={discovered}
            onSent={onSent}
            onWalletError={onWalletError}
          />
        )}
        {method === "qr" && (
          <div className="pp-qr-panel">
            <QrCode value={quote.payment_uri} label={`Payment request for ${amount}`} />
            <p className="pp-message">
              Scan with your wallet app and check it shows {amount} on {networkName(quote.chain_id)}.
            </p>
          </div>
        )}
        {method === "manual" && <ManualPanel quote={quote} />}
      </div>
    </>
  );
}

type WalletStep =
  | { kind: "idle" }
  | { kind: "pending"; wallet: string }
  | { kind: "failed"; message: string };

function WalletPanel({
  quote,
  buttonText,
  walletClient,
  discovered,
  onSent,
  onWalletError,
}: {
  quote: ClientQuote;
  discovered: Wallet[];
  buttonText: string;
  walletClient: WalletClient | undefined;
  onSent: (hash: Hash) => void;
  onWalletError: CheckoutProps["onWalletError"];
}) {
  const [step, setStep] = useState<WalletStep>({ kind: "idle" });
  const wallets: { id: string; name: string; icon: string; wallet: WalletClient | EthereumProvider }[] =
    walletClient === undefined
      ? discovered.map(({ info: { uuid, name, icon }, provider }) => ({ id: uuid, name, icon, wallet: provider }))
      : [{ id: "client", name: "your wallet", icon: "", wallet: walletClient }];

  const pay = async ({ name, wallet }: (typeof wallets)[number]) => {
    setStep({ kind: "pending", wallet: name });
    try {
      onSent(await payWithWallet(wallet, quote));
      setStep({ kind: "idle" });
    } catch (error) {
      setStep({
        kind: "failed",
        message: error instanceof WalletError ? error.message : "The payment could not be sent",
      });
      if (error instanceof WalletError) {
        onWalletError?.(error, wallet);
      }
    }
  };

  if (wallets.length === 0) {
    return (
      <p className="pp-message">
        Install a browser wallet to pay here.
      </p>
    );
  }
  return (
    <div className="pp-wallets">
      {wallets.map((choice) => (
        <button
          key={choice.id}
          type="button"
          className="pp-button"
          disabled={step.kind === "pending"}
          onClick={() => void pay(choice)}
          aria-label={walletClient === undefined ? `${buttonText} (${choice.name})` : buttonText}
        >
          {choice.icon !== "" && <img src={choice.icon} alt="" />}
          <span>{buttonText}</span>
          {walletClient === undefined && <span className="pp-wallet-name">{choice.name}</span>}
        </button>
      ))}
      <p
        className="pp-message"
        data-tone={step.kind === "failed" ? "danger" : undefined}
        aria-live="polite"
      >
        {step.kind === "pending" && `Confirm the payment in ${step.wallet}…`}
        {step.kind === "failed" && step.message}
      </p>
    </div>
  );
}

function ManualPanel({ quote }: { quote: ClientQuote }) {
  const token = quoteTransfer(quote).token;
  return (
    <>
      <dl className="pp-fields">
        <Field label="Network" value={`${networkName(quote.chain_id)} (chain ID ${quote.chain_id})`} />
        <Field label={`Token (${quote.asset.toUpperCase()}) contract`} value={token} copy />
        <Field label="Send to address" value={quote.address} copy />
        <Field label="Exact amount" value={formatTokenAmount(quote)} copy={tokenAmount(quote)} />
      </dl>
      <p className="pp-message">
        Only {quote.asset.toUpperCase()} on {networkName(quote.chain_id)} is credited. Exchange
        withdrawal fees must not reduce the amount received.
      </p>
    </>
  );
}

/** The current time in milliseconds, updated every second while `active`. */
function useNow(active: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) {
      return;
    }
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [active]);
  return now;
}
