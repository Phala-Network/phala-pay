"use client";

import { useEffect, useId, useRef, useState, type KeyboardEvent } from "react";
import { isHash, type Hash, type WalletClient } from "viem";
import type { CheckoutErrorCode, CheckoutState, CheckoutStatus } from "@phala/pay";
import { networkName, transactionUrl } from "@phala/pay";
import {
  formatAmount,
  formatCountdown,
  formatMinorAmount,
  formatTokenAmount,
  formatWait,
  tokenAmount,
} from "@phala/pay";
import { quoteTransfer } from "@phala/pay";
import type { ClientQuote } from "@phala/pay";
import { WalletError, payWithWallet, type EthereumProvider } from "@phala/pay";
import { rootProps, type Appearance } from "./appearance.js";
import { AssetIcon, NetworkIcon } from "./Icons.js";
import { CopyButton, Field } from "./Field.js";
import { ExternalGlyph, ToneGlyph } from "./Glyphs.js";
import { QrCode } from "./QrCode.js";
import { useCheckout } from "./useCheckout.js";
import { useWallets, type WalletChoice } from "./wallets.js";

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
  onSuccess?: ((quote: ClientQuote) => void) | undefined;
  /** Called once when the quote expires or is canceled without a payment. */
  onExpire?: ((quote: ClientQuote) => void) | undefined;
  /** Called whenever the checkout's status or `txHash` changes, like Stripe Elements' `onChange`:
   * for example, to hide your own "new payment" control while a payment is sent, `seen`, or
   * `confirming`, and to keep `txHash` with the client secret for a reload. */
  onChange?: (state: CheckoutChange) => void;
  /** A payment already sent for this quote, the `txHash` that `onChange` reported, to resume after
   * a reload: the checkout follows it instead of offering to pay again. */
  txHash?: string | undefined;
  /** Called when the wallet tab could not send the payment, with the `WalletError` and the wallet
   * it tried (the chosen browser wallet's provider, or `walletClient`): for example on
   * `insufficient_balance` (nothing was sent), to offer your own way to fund that wallet. The
   * checkout shows the error's message itself. */
  onWalletError?: ((error: WalletError, wallet: WalletClient | EthereumProvider) => void) | undefined;
  appearance?: Appearance;
  /** Milliseconds between status reads; default 3000. */
  pollInterval?: number | undefined;
  className?: string;
  /** The wallet button's label, followed by the wallet's name; default "Pay with <wallet name>", or
   * "Pay with your wallet" for `walletClient`. */
  buttonText?: string;
  /** Your page's connected wallet, for example wagmi's `useWalletClient().data`. When set, the
   * wallet tab pays with it and its account instead of listing the browser's wallets. */
  walletClient?: WalletClient | undefined;
}

/** What `<Checkout onChange>` receives: the quote's state, and the payment the payer sent. */
export interface CheckoutChange extends CheckoutState {
  /** The transaction the wallet sent for this quote, or `txHash` on resume; `null` before then.
   * Until the status is final, there is a payment on its way: do not let the payer pay again. */
  txHash: string | null;
}

type Method = "wallet" | "qr" | "manual";

const METHODS: { id: Method; label: string }[] = [
  { id: "wallet", label: "Wallet" },
  { id: "qr", label: "QR code" },
  { id: "manual", label: "Manual" },
];

/** A checkout for one quote: pay from a browser wallet, by QR code, or manually, with live status. */
export function Checkout({ appearance, className, ...props }: CheckoutProps) {
  return (
    <div {...rootProps(appearance, className)}>
      <CheckoutContent {...props} />
    </div>
  );
}

/** The checkout without its `.pp-root`, for a component that is itself inside one. */
export function CheckoutContent({
  clientSecret,
  expectedAddress,
  apiBase,
  onSuccess,
  onExpire,
  onChange,
  onWalletError,
  pollInterval,
  buttonText,
  walletClient,
  txHash: resumed,
}: Omit<CheckoutProps, "appearance" | "className">) {
  const { status, quote, error, reconnecting, refresh } = useCheckout({
    clientSecret,
    expectedAddress,
    apiBase,
    ...(pollInterval === undefined ? {} : { pollInterval }),
  });
  // The payment sent from the wallet, for this quote only.
  const [sent, setSent] = useState<{ clientSecret: string; hash: Hash } | null>(null);
  const txHash =
    sent?.clientSecret === clientSecret
      ? sent.hash
      : resumed !== undefined && isHash(resumed)
        ? resumed
        : null;
  const now = useNow(status === "waiting");

  const callbacks = useRef({ onSuccess, onExpire, onChange });
  const state = useRef<CheckoutChange>({ status, quote, error, txHash });
  useEffect(() => {
    callbacks.current = { onSuccess, onExpire, onChange };
    state.current = { status, quote, error, txHash };
  });
  // Once per status or payment change, not on every poll.
  useEffect(() => {
    callbacks.current.onChange?.(state.current);
  }, [status, txHash]);
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
    <>
      {status === "loading" && (
        <div className="pp-skeleton" aria-hidden="true">
          <span className="pp-skeleton__line" />
          <span className="pp-skeleton__line" />
        </div>
      )}
      {quote !== null && (
        <div className="pp-summary">
          <p className="pp-summary__amount">
            <AssetIcon asset={quote.asset} size={20} />
            {formatTokenAmount(quote)} {quote.asset.toUpperCase()}
          </p>
          <p className="pp-summary__meta">
            {formatAmount(quote)} ·{" "}
            <span className="pp-summary__network">
              <NetworkIcon chainId={quote.chain_id} size={16} />
              {networkName(quote.chain_id)}
            </span>
            {!quote.livemode && <> · <span className="pp-summary__test-mode">Test mode</span></>}
          </p>
        </div>
      )}
      <StatusLine
        status={status}
        quote={quote}
        now={now}
        error={error}
        reconnecting={reconnecting ?? false}
        sent={txHash !== null}
      />
      {txHash !== null && quote !== null && <Transaction hash={txHash} chainId={quote.chain_id} />}
      {/* Once a payment is sent, nothing offers to pay again: paying again would pay twice. */}
      {status === "waiting" && quote !== null && txHash === null && (
        <PaymentOptions
          quote={quote}
          buttonText={buttonText}
          walletClient={walletClient}
          onSent={(hash) => {
            setSent({ clientSecret, hash });
            refresh();
          }}
          onWalletError={onWalletError}
        />
      )}
    </>
  );
}

function StatusLine({
  status,
  quote,
  now,
  error,
  reconnecting,
  sent,
}: {
  status: CheckoutStatus;
  quote: ClientQuote | null;
  now: number;
  error: CheckoutState["error"];
  reconnecting: boolean;
  sent: boolean;
}) {
  const tone: "neutral" | "success" | "danger" =
    status === "credited"
      ? "success"
      : ["rejected", "reversed", "expired", "canceled", "error"].includes(status)
        ? "danger"
        : "neutral";
  return (
    <div className="pp-status" data-tone={tone}>
      <span className="pp-status__text" role="status" aria-live="polite">
        {/* One plain line while in progress; a mark only for its outcome, which arrives with its
            own text (a new line, by its tone) rather than moving the text already there. */}
        <span className="pp-status__line" key={tone}>
          {tone !== "neutral" && (
            <span className="pp-status__icon">
              <ToneGlyph tone={tone} />
            </span>
          )}
          <span>
            {status === "waiting" && sent ? "Sent, waiting for confirmation" : statusMessage(status, quote, error?.code)}
            {(reconnecting || error !== null) && status !== "error" && " (reconnecting…)"}
          </span>
        </span>
      </span>
      {status === "waiting" && quote !== null && (
        <span className="pp-status__countdown" role="timer" aria-label="Time left to pay">
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
      return "Payment reversed: a chain reorganization replaced it and its credit was removed. Check your wallet.";
    case "expired":
      return "Quote expired. Do not send funds.";
    case "canceled":
      return "Quote canceled. Do not send funds.";
    case "error":
      return code === "address_mismatch"
        ? "This payment address could not be verified. Do not send funds; contact support."
        : "This payment link is not valid.";
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

/** When the payment was valued at the market price, what was credited and the quote it missed. */
function creditedMessage(quote: ClientQuote | null): string {
  if (quote?.amount_credited == null || quote.amount_credited === quote.amount) {
    return "Payment credited";
  }
  const credited = formatMinorAmount(quote, quote.amount_credited);
  if (quote.amount_credited < quote.amount) {
    return `Payment credited: ${credited} of ${formatAmount(quote)}`;
  }
  return `Payment credited: ${credited} (${formatAmount(quote)} quoted)`;
}

function Transaction({ hash, chainId }: { hash: Hash; chainId: number }) {
  const url = transactionUrl(chainId, hash);
  return (
    <p className="pp-tx">
      <span className="pp-tx__text">
        Transaction sent:{" "}
        {url === undefined ? (
          <span className="pp-tx__hash">{hash}</span>
        ) : (
          <a className="pp-tx__hash" href={url} target="_blank" rel="noreferrer">
            {hash}
            <ExternalGlyph />
          </a>
        )}
      </span>
      <CopyButton value={hash} label="transaction hash" />
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
  buttonText: string | undefined;
  walletClient: WalletClient | undefined;
  onSent: (hash: Hash) => void;
  onWalletError: CheckoutProps["onWalletError"];
}) {
  const [selectedMethod, setMethod] = useState<Method | null>(null);
  const wallets = useWallets(walletClient);
  const method = selectedMethod ?? (wallets.length > 0 ? "wallet" : "qr");
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
        Send <strong>exactly {amount}</strong> before {new Date(quote.expires_at * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })} for this price; anything else
        sent to this address is credited at the market rate.
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
            className="pp-tabs__tab"
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
        className="pp-tabs__panel"
        role="tabpanel"
        id={`${id}-panel-${method}`}
        aria-labelledby={`${id}-tab-${method}`}
        tabIndex={0}
      >
        {method === "wallet" && (
          <WalletPanel
            quote={quote}
            buttonText={buttonText}
            wallets={wallets}
            named={walletClient === undefined}
            onSent={onSent}
            onWalletError={onWalletError}
          />
        )}
        {method === "qr" && (
          <div className="pp-qr">
            <QrCode value={quote.payment_uri} label={`Payment request for ${amount}`} />
            <p className="pp-qr__caption">Scan with your wallet app.</p>
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
  wallets,
  named,
  onSent,
  onWalletError,
}: {
  quote: ClientQuote;
  wallets: WalletChoice[];
  buttonText: string | undefined;
  /** Whether each button names its wallet: not for the page's own wallet client. */
  named: boolean;
  onSent: (hash: Hash) => void;
  onWalletError: CheckoutProps["onWalletError"];
}) {
  const [step, setStep] = useState<WalletStep>({ kind: "idle" });

  const pay = async ({ name, wallet }: WalletChoice) => {
    setStep({ kind: "pending", wallet: name });
    try {
      // The checkout then follows the payment in place of this panel.
      onSent(await payWithWallet(wallet, quote));
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
        >
          {choice.icon !== "" && <img className="pp-button__icon" src={choice.icon} alt="" />}
          {buttonText === undefined ? (
            <span>Pay with {choice.name}</span>
          ) : (
            <>
              <span>{buttonText}</span>
              {named && (
                <>
                  {" "}
                  <span className="pp-button__detail">{choice.name}</span>
                </>
              )}
            </>
          )}
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
  const symbol = quote.asset.toUpperCase();
  return (
    <>
      <dl className="pp-fields">
        <Field label="Network" value={`${networkName(quote.chain_id)} (chain ID ${quote.chain_id})`} />
        <Field label={`Token (${symbol}) contract`} value={token} copy />
        <Field label="Send to address" value={quote.address} copy />
        <Field label="Exact amount" value={`${formatTokenAmount(quote)} ${symbol}`} copy={tokenAmount(quote)} />
      </dl>
      <p className="pp-message">Exchange withdrawal fees must not reduce the amount received.</p>
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
