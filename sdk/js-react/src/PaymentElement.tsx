"use client";

import { useEffect, useRef, useState, type ReactNode, type SyntheticEvent } from "react";
import type { WalletClient } from "viem";
import {
  assetSymbol,
  samePaymentOption,
  type CheckoutParams,
  type ClientQuote,
  type EthereumProvider,
  type PaymentOption,
  type WalletError,
} from "@phala/pay";
import { rootProps, type Appearance } from "./appearance.js";
import { CheckoutContent, type CheckoutChange } from "./Checkout.js";
import { PaymentOptionFields } from "./PaymentOptionPicker.js";

/** What `createQuote` resolves to: your backend's `pay.checkoutParams(quote)`. */
export type PaymentElementQuote = Omit<CheckoutParams, "apiBase"> & { apiBase?: string };

/** What `createQuote` receives beside the option. */
export interface PaymentElementQuoteRequest {
  /**
   * Send it as your backend's `Idempotency-Key` for this quote. It is new for each quote the payer
   * asks for, and the same only when they retry after `createQuote` failed with the same option,
   * `amount`, and `currency`, so a retry returns the quote a lost response created rather than a
   * second one. A host that varies any other field of the quote between attempts must pass it as
   * `amount` or remount the element (a new `key`), or the retry reuses a key with another body.
   */
  idempotencyKey: string;
}

/** Where the payer is: choosing, waiting for your backend, or paying a quote. */
export type PaymentElementStep = "options" | "creating" | "checkout";

export interface PaymentElementState {
  step: PaymentElementStep;
  /**
   * The quote's checkout state while `step` is `checkout`, with the `txHash` the wallet sent;
   * otherwise `null`. Keep `txHash` with the client secret to resume with `<Checkout txHash>`.
   */
  checkout: CheckoutChange | null;
}

export interface PaymentElementProps<T extends PaymentOption> {
  /** The tokens and networks to offer, in order: `paymentOptions()` of your `GET /v1/config` assets. */
  options: readonly T[];
  /** The chosen option, when you control it; compared by chain and asset code. */
  value?: PaymentOption | null | undefined;
  /** The option chosen first when you do not control it; the first option by default. */
  defaultValue?: PaymentOption | null | undefined;
  /** Called with the payer's choice: the very object of `options` they chose. */
  onOptionChange?: ((option: T) => void) | undefined;
  /** The amount in minor units of `currency` (cents), for the button: "Pay $10.00 in USDC". */
  amount?: number | undefined;
  /** The amount's ISO currency code; default `usd`. */
  currency?: string | undefined;
  /**
   * Called once per submit, with the chosen option and the idempotency key to create its quote
   * with: create the quote on your backend with its API key and that `Idempotency-Key`, and resolve
   * with `pay.checkoutParams(quote)`. A rejection is shown with a way to retry.
   */
  createQuote: (option: T, request: PaymentElementQuoteRequest) => Promise<PaymentElementQuote>;
  /** The service origin, for a quote that does not name its own. */
  apiBase?: string | undefined;
  /** The form's id: a button elsewhere on your page with `type="submit" form={id}` submits it. */
  id?: string | undefined;
  /** Hides the element's own submit button, when your button (see `id`, or `children`) submits it. */
  hideSubmit?: boolean | undefined;
  /**
   * Your content inside the form, under the token and network, while the payer chooses: a summary
   * above the element's button, or with `hideSubmit` your summary and your `type="submit"` button,
   * in the same form.
   */
  children?: ReactNode;
  disabled?: boolean | undefined;
  /** The options' visible label; default "Pay with". */
  label?: ReactNode | undefined;
  /** Text at the end of an option, such as its price or a bonus; keep it non-interactive. */
  renderDetail?: ((option: T) => ReactNode) | undefined;
  /** Called on every step and, while paying a quote, on every checkout status or `txHash` change. */
  onChange?: ((state: PaymentElementState) => void) | undefined;
  /** Called once when the quote's payment is credited. Fulfil from the `deposit.credited` webhook. */
  onSuccess?: ((quote: ClientQuote) => void) | undefined;
  /** Called once when the quote expires or is canceled without a payment. */
  onExpire?: ((quote: ClientQuote) => void) | undefined;
  /** Called when `createQuote` rejects, with its error. */
  onError?: ((error: unknown) => void) | undefined;
  /**
   * The alert to show when `createQuote` rejects, from its error, for
   * example your backend's reason; `null`, `undefined`, or an empty string keeps the element's own,
   * such as "Could not start the payment. Try again."
   */
  errorMessage?: ((cause: unknown) => string | null | undefined) | undefined;
  /** As `<Checkout onWalletError>`. */
  onWalletError?: ((error: WalletError, wallet: WalletClient | EthereumProvider) => void) | undefined;
  /** As `<Checkout walletClient>`. */
  walletClient?: WalletClient | undefined;
  /** Milliseconds between status reads; default 3000. */
  pollInterval?: number | undefined;
  appearance?: Appearance | undefined;
  className?: string | undefined;
}

type Step =
  | { step: "options"; failed: string | null }
  | { step: "creating" }
  | { step: "checkout"; params: CheckoutParams };

// While a payment is on its way, the checkout stays: leaving it would hide the payment's progress.
const UNDERWAY = new Set(["seen", "confirming"]);
const FINISHED = new Set(["credited", "rejected", "reversed", "expired", "canceled", "error"]);

/**
 * A complete payment, created only when the payer submits, like Stripe's Payment Element with a
 * deferred intent: the payer picks a token and network, submits, and your `createQuote` creates
 * the quote on your backend; the element then shows its checkout (wallet, QR code, or manual,
 * with live status), and a way back to the options before a payment is sent or once the quote is
 * final. The quote uses the customer's fixed deposit address.
 */
export function PaymentElement<T extends PaymentOption>({
  options,
  value,
  defaultValue,
  onOptionChange,
  amount,
  currency = "usd",
  createQuote,
  apiBase,
  id,
  hideSubmit = false,
  children,
  disabled = false,
  label,
  renderDetail,
  onChange,
  onSuccess,
  onExpire,
  onError,
  errorMessage,
  onWalletError,
  walletClient,
  pollInterval,
  appearance,
  className,
}: PaymentElementProps<T>) {
  const [current, setCurrent] = useState<Step>({ step: "options", failed: null });
  const [chosen, setChosen] = useState<PaymentOption | null>(defaultValue === undefined ? (options[0] ?? null) : defaultValue);
  const [checkout, setCheckout] = useState<CheckoutChange | null>(null);
  const selected = value === undefined ? chosen : value;
  const option = options.find((each) => samePaymentOption(each, selected));

  // The request in flight: a submit while one is, or the result of one the payer left, is ignored.
  const request = useRef(0);
  const pending = useRef(false);
  // The idempotency key of the quote being asked for: kept while `createQuote` fails for the same
  // option and amount, so that its retry is the same request; dropped once a quote is created.
  const attempt = useRef<{ option: PaymentOption; amount: number | undefined; currency: string; key: string } | null>(null);
  const callbacks = useRef({ onChange, onError, errorMessage });
  useEffect(() => {
    callbacks.current = { onChange, onError, errorMessage };
  });
  const step = current.step;
  useEffect(() => {
    callbacks.current.onChange?.({
      step,
      checkout: step === "checkout" ? checkout : null,
    });
  }, [step, checkout]);
  // A new step takes the focus, so that a keyboard or screen reader user continues from its start.
  const body = useRef<HTMLDivElement>(null);
  const shown = useRef(step);
  useEffect(() => {
    if (shown.current !== step) {
      shown.current = step;
      if (step !== "creating") body.current?.focus();
    }
  }, [step]);
  useEffect(() => () => {
    request.current += 1;
    pending.current = false;
  }, []);

  const start = <R,>(create: () => Promise<R>, done: (result: R) => Step) => {
    if (pending.current) {
      return;
    }
    pending.current = true;
    const mine = ++request.current;
    setCurrent({ step: "creating" });
    // A `create` that throws rejects, as one whose promise does.
    new Promise<R>((resolve) => resolve(create())).then(
      (result) => {
        if (mine !== request.current) return;
        pending.current = false;
        setCheckout(null);
        setCurrent(done(result));
      },
      (error: unknown) => {
        if (mine !== request.current) return;
        pending.current = false;
        setCurrent({ step: "options", failed: failureMessage(error, callbacks.current.errorMessage) });
        callbacks.current.onError?.(error);
      },
    );
  };

  const submit = (event: SyntheticEvent) => {
    event.preventDefault();
    if (disabled || option === undefined) {
      return;
    }
    const same =
      attempt.current !== null &&
      samePaymentOption(attempt.current.option, option) &&
      attempt.current.amount === amount &&
      attempt.current.currency === currency;
    if (attempt.current === null || !same) {
      attempt.current = { option, amount, currency, key: crypto.randomUUID() };
    }
    const { key } = attempt.current;
    start(() => createQuote(option, { idempotencyKey: key }), (params): Step => {
      attempt.current = null;
      return { step: "checkout", params: { ...params, apiBase: params.apiBase ?? apiBase ?? "" } };
    });
  };

  const back = () => {
    request.current += 1;
    pending.current = false;
    setCheckout(null);
    setCurrent({ step: "options", failed: null });
  };

  const status = checkout?.status ?? "loading";
  // Once a payment is sent there is no way back until it is final: paying again would pay twice.
  const backText = FINISHED.has(status)
    ? "Start a new payment"
    : UNDERWAY.has(status) || (checkout !== null && checkout.txHash !== null)
      ? null
      : "Back";
  const symbol = option === undefined ? null : assetSymbol(option.asset);
  const submitText =
    symbol === null ? "Pay" : amount === undefined ? `Pay in ${symbol}` : `Pay ${formatMoney(amount, currency)} in ${symbol}`;

  return (
    <div {...rootProps(appearance, className)}>
      <div className="pp-element__step" ref={body} tabIndex={-1}>
        {(current.step === "options" || current.step === "creating") && (
          <form id={id} className="pp-element" onSubmit={submit} aria-busy={current.step === "creating"} noValidate>
            <PaymentOptionFields
              options={options}
              value={option}
              onChange={(each) => {
                setChosen(each);
                onOptionChange?.(each);
              }}
              disabled={disabled || current.step === "creating"}
              label={label}
              renderDetail={renderDetail}
            />
            {children}
            {!hideSubmit && (
              <button
                type="submit"
                className="pp-button"
                disabled={
                  disabled ||
                  option === undefined ||
                  current.step === "creating"
                }
              >
                <span>{current.step === "creating" ? "Starting your payment…" : submitText}</span>
              </button>
            )}
            {current.step === "options" && current.failed !== null && (
              <p className="pp-message" data-tone="danger" role="alert">
                {current.failed}
              </p>
            )}
          </form>
        )}
        {current.step === "checkout" && (
          <>
            {backText !== null && (
              // Each way back is its own control, not the previous one relabelled, and above the
              // checkout, which changes as the payment does.
              <p className="pp-element__back" key={backText}>
                <button type="button" className="pp-text-button" onClick={back}>
                  {backText}
                </button>
              </p>
            )}
            <CheckoutContent
              {...current.params}
              onChange={setCheckout}
              onSuccess={onSuccess}
              onExpire={onExpire}
              onWalletError={onWalletError}
              walletClient={walletClient}
              pollInterval={pollInterval}
            />
          </>
        )}
      </div>
    </div>
  );
}

/** The alert for a rejected quote request: the host's `errorMessage` for it, else the element's own. */
function failureMessage(
  cause: unknown,
  errorMessage: PaymentElementProps<PaymentOption>["errorMessage"],
): string {
  let message: string | null | undefined;
  try {
    message = errorMessage?.(cause);
  } catch (error) {
    // A throwing callback is the host's bug: report it, and still show the element's own alert.
    queueMicrotask(() => {
      throw error;
    });
  }
  if (typeof message === "string" && message.trim() !== "") {
    return message;
  }
  return "Could not start the payment. Try again.";
}

/** `minor` units of `currency`, for example `$10.00` for 1000 cents. */
function formatMoney(minor: number, currency: string): string {
  const format = new Intl.NumberFormat("en-US", { style: "currency", currency: currency.toUpperCase() });
  return format.format(minor / 10 ** (format.resolvedOptions().maximumFractionDigits ?? 2));
}
