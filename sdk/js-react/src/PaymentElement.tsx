"use client";

import { useEffect, useRef, useState, type ReactNode, type SyntheticEvent } from "react";
import type { WalletClient } from "viem";
import {
  assetSymbol,
  samePaymentOption,
  type CheckoutParams,
  type CheckoutState,
  type ClientQuote,
  type DepositAddressAsset,
  type DepositAddressDetails,
  type DepositAddressPayment,
  type EthereumProvider,
  type PaymentOption,
  type WalletError,
} from "@phala/pay";
import { rootProps, type Appearance } from "./appearance.js";
import { CheckoutContent } from "./Checkout.js";
import { DepositAddressContent } from "./DepositAddress.js";
import { PaymentOptionFields } from "./PaymentOptionPicker.js";

/** What `createQuote` resolves to: your backend's `pay.checkoutParams(quote)`. */
export type PaymentElementQuote = Omit<CheckoutParams, "apiBase"> & { apiBase?: string };

/**
 * What `createDepositAddress` resolves to: `address` and `networks` of your backend's
 * `POST /v1/deposit_addresses` response, with its `client_secret` to follow payments.
 */
export type PaymentElementDepositAddress = DepositAddressDetails & { clientSecret?: string; apiBase?: string };

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

/** Where the payer is: choosing, waiting for your backend, paying a quote, or at a deposit address. */
export type PaymentElementStep = "options" | "creating" | "checkout" | "deposit_address";

export interface PaymentElementState {
  step: PaymentElementStep;
  /** The quote's checkout state while `step` is `checkout`; otherwise `null`. */
  checkout: CheckoutState | null;
  /**
   * While `step` is `deposit_address` with an amount (`depositAmountAtomic`), the payment it
   * follows, once there is one; otherwise `null`.
   */
  payment: DepositAddressPayment | null;
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
   * with `pay.checkoutParams(quote)`. A rejection is shown with a way to retry. Without it, submit
   * shows the customer's deposit address (`createDepositAddress`) instead.
   */
  createQuote?: ((option: T, request: PaymentElementQuoteRequest) => Promise<PaymentElementQuote>) | undefined;
  /**
   * Get the customer's deposit address from your backend, with the chosen option to show first;
   * give one of this and `createQuote`. With `createQuote`, it is offered as one line at the end of
   * the form, "Paying from an exchange? Use a deposit address", and the address states its
   * per-transfer limits. Without it, submit calls it: the customer's one fixed address is how they
   * pay, best with `depositAmountAtomic`.
   */
  createDepositAddress?: ((option: T | undefined) => Promise<PaymentElementDepositAddress>) | undefined;
  /**
   * The token amount for the deposit address to ask for, in the token's smallest unit; `undefined`
   * for any amount. Called once `createDepositAddress` resolves, with the chosen option and that
   * address's token for it on the chosen network, whose `decimals` give the unit: for example
   * `(2500n * 10n ** BigInt(token.decimals)) / 100n` for $25.00 of a $1 stablecoin. With an amount,
   * the address view states it once, pays it from a wallet first (a QR code and the address are
   * the secondary way), follows that payment, and reports it in `onChange` and `onDepositSuccess`.
   */
  depositAmountAtomic?: ((option: T, token: DepositAddressAsset) => bigint | undefined) | undefined;
  /**
   * A deposit-address payment already sent from a wallet, from `onDepositSent`, to resume after a
   * reload: the element calls `createDepositAddress` with the chosen option at once (set `value` or
   * `defaultValue` to the option it was paid in, on `chainId`) and follows that transaction.
   */
  depositTxHash?: { txHash: string; chainId: number } | undefined;
  /** The service origin, for a quote or deposit address that does not name its own. */
  apiBase?: string | undefined;
  /** The form's id: a button elsewhere on your page with `type="submit" form={id}` submits it. */
  id?: string | undefined;
  /** Hides the element's own submit button, when your button (see `id`, or `children`) submits it. */
  hideSubmit?: boolean | undefined;
  /**
   * Your content inside the form, under the token and network, while the payer chooses: a summary
   * above the element's button, or with `hideSubmit` your summary and your `type="submit"` button,
   * which then comes before the deposit-address line.
   */
  children?: ReactNode;
  disabled?: boolean | undefined;
  /** The options' visible label; default "Pay with". */
  label?: ReactNode | undefined;
  /** Text at the end of an option, such as its price or a bonus; keep it non-interactive. */
  renderDetail?: ((option: T) => ReactNode) | undefined;
  /** Called on every step and, while paying a quote, on every checkout status change. */
  onChange?: ((state: PaymentElementState) => void) | undefined;
  /** Called once when the quote's payment is credited. Fulfil from the `deposit.credited` webhook. */
  onSuccess?: ((quote: ClientQuote) => void) | undefined;
  /** Called once when the quote expires or is canceled without a payment. */
  onExpire?: ((quote: ClientQuote) => void) | undefined;
  /**
   * Called once when the deposit address's payment, with `depositAmountAtomic`, is credited.
   * Fulfil from the `deposit.credited` webhook.
   */
  onDepositSuccess?: ((payment: DepositAddressPayment) => void) | undefined;
  /** Called once the wallet has sent the deposit-address payment: keep both for `depositTxHash`. */
  onDepositSent?: ((txHash: string, chainId: number) => void) | undefined;
  /** Called when `createQuote` or `createDepositAddress` rejects, with its error. */
  onError?: ((error: unknown) => void) | undefined;
  /**
   * The alert to show when `createQuote` or `createDepositAddress` rejects, from its error, for
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
  | { step: "creating"; kind: "quote" | "deposit_address" }
  | { step: "checkout"; params: CheckoutParams }
  | {
      step: "deposit_address";
      created: PaymentElementDepositAddress;
      apiBase: string | undefined;
      option: PaymentOption | undefined;
      amountAtomic: bigint | undefined;
      txHash: string | undefined;
    };

// While a payment is on its way, the checkout stays: leaving it would hide the payment's progress.
const UNDERWAY = new Set(["seen", "confirming"]);
const FINISHED = new Set(["credited", "rejected", "reversed", "expired", "canceled", "error"]);

/**
 * A complete payment, created only when the payer submits, like Stripe's Payment Element with a
 * deferred intent: the payer picks a token and network, submits, and your `createQuote` creates
 * the quote on your backend; the element then shows its checkout (wallet, QR code, or manual,
 * with live status), and a way back to the options once the quote can no longer be paid or before
 * a payment is sent. With `createDepositAddress`, it also offers the customer's deposit address,
 * or, without `createQuote`, shows that address on submit: the customer's one fixed address, with
 * `depositAmountAtomic` paid from a wallet first.
 */
export function PaymentElement<T extends PaymentOption>({
  options,
  value,
  defaultValue,
  onOptionChange,
  amount,
  currency = "usd",
  createQuote,
  createDepositAddress,
  depositAmountAtomic,
  depositTxHash,
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
  onDepositSuccess,
  onDepositSent,
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
  const [checkout, setCheckout] = useState<CheckoutState | null>(null);
  const [payment, setPayment] = useState<DepositAddressPayment | null>(null);
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
      payment: step === "deposit_address" ? payment : null,
    });
  }, [step, checkout, payment]);
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

  const start = <R,>(kind: "quote" | "deposit_address", create: () => Promise<R>, done: (result: R) => Step) => {
    if (pending.current) {
      return;
    }
    pending.current = true;
    const mine = ++request.current;
    setCurrent({ step: "creating", kind });
    // A `create` that throws rejects, as one whose promise does.
    new Promise<R>((resolve) => resolve(create())).then(
      (result) => {
        if (mine !== request.current) return;
        pending.current = false;
        setCheckout(null);
        setPayment(null);
        setCurrent(done(result));
      },
      (error: unknown) => {
        if (mine !== request.current) return;
        pending.current = false;
        setCurrent({ step: "options", failed: failureMessage(kind, error, callbacks.current.errorMessage) });
        callbacks.current.onError?.(error);
      },
    );
  };

  const submit = (event: SyntheticEvent) => {
    event.preventDefault();
    if (disabled || option === undefined) {
      return;
    }
    if (createQuote === undefined) {
      showDepositAddress();
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
    start("quote", () => createQuote(option, { idempotencyKey: key }), (params): Step => {
      attempt.current = null;
      return { step: "checkout", params: { ...params, apiBase: params.apiBase ?? apiBase ?? "" } };
    });
  };

  const showDepositAddress = (txHash?: string) => {
    if (disabled || createDepositAddress === undefined) {
      return;
    }
    const create = async () => {
      const created = await createDepositAddress(option);
      if (option === undefined || depositAmountAtomic === undefined) {
        return { created, amountAtomic: undefined };
      }
      // An amount is of the chosen token: the address must take it on the chosen network.
      const token = created.networks
        .find((each) => each.chain_id === option.chain_id)
        ?.assets.find((each) => each.asset === option.asset);
      if (token === undefined) {
        throw new TypeError("the deposit address does not take the chosen token on the chosen network");
      }
      return { created, amountAtomic: depositAmountAtomic(option, token) };
    };
    start("deposit_address", create, ({ created, amountAtomic }): Step => ({
      step: "deposit_address",
      created,
      apiBase: created.apiBase ?? apiBase,
      option,
      amountAtomic,
      txHash,
    }));
  };

  // A payment sent before a reload: back to its address at once, following it.
  const resume = depositTxHash !== undefined && option?.chain_id === depositTxHash.chainId ? depositTxHash.txHash : undefined;
  const resumeOnMount = useRef(resume);
  const showOnMount = useRef(showDepositAddress);
  useEffect(() => {
    if (resumeOnMount.current !== undefined) showOnMount.current(resumeOnMount.current);
  }, []);

  const back = () => {
    request.current += 1;
    pending.current = false;
    setCheckout(null);
    setPayment(null);
    setCurrent({ step: "options", failed: null });
  };

  const status = checkout?.status ?? payment?.status ?? "loading";
  const backText = UNDERWAY.has(status)
    ? null
    : status === "credited"
      ? "Start a new payment"
      : FINISHED.has(status)
        ? "Start over"
        : "Choose another token or network";
  const depositBackText = payment === null ? "Back to payment options" : backText;
  const primary = createQuote === undefined ? "deposit_address" : "quote";
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
                  current.step === "creating" ||
                  (createQuote === undefined && createDepositAddress === undefined)
                }
              >
                <span>{current.step === "creating" && current.kind === primary ? "Starting your payment…" : submitText}</span>
              </button>
            )}
            {current.step === "options" && current.failed !== null && (
              <p className="pp-message" data-tone="danger" role="alert">
                {current.failed}
              </p>
            )}
            {createQuote !== undefined && createDepositAddress !== undefined && (
              <p className="pp-element__alternative">
                Paying from an exchange?{" "}
                <button
                  type="button"
                  className="pp-text-button"
                  onClick={() => showDepositAddress()}
                  disabled={disabled || current.step === "creating"}
                >
                  {current.step === "creating" && current.kind === "deposit_address"
                    ? "Getting your deposit address…"
                    : "Use a deposit address"}
                </button>
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
        {current.step === "deposit_address" && (
          <>
            {depositBackText !== null && (
              <p className="pp-element__back" key={depositBackText}>
                <button type="button" className="pp-text-button" onClick={back}>
                  {depositBackText}
                </button>
              </p>
            )}
            <DepositAddressContent
              depositAddress={{ address: current.created.address, networks: current.created.networks }}
              chainId={current.option?.chain_id}
              asset={current.option?.asset}
              clientSecret={current.created.clientSecret}
              apiBase={current.apiBase}
              pollInterval={pollInterval}
              amountAtomic={current.amountAtomic}
              walletClient={walletClient}
              onWalletError={onWalletError}
              onPaymentChange={setPayment}
              onSuccess={onDepositSuccess}
              onSent={onDepositSent}
              txHash={current.txHash}
            />
          </>
        )}
      </div>
    </div>
  );
}

/** The alert for a rejected `kind` of request: the host's `errorMessage` for it, else the element's own. */
function failureMessage(
  kind: "quote" | "deposit_address",
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
  return kind === "quote" ? "Could not start the payment. Try again." : "Could not get your deposit address. Try again.";
}

/** `minor` units of `currency`, for example `$10.00` for 1000 cents. */
function formatMoney(minor: number, currency: string): string {
  const format = new Intl.NumberFormat("en-US", { style: "currency", currency: currency.toUpperCase() });
  return format.format(minor / 10 ** (format.resolvedOptions().maximumFractionDigits ?? 2));
}
