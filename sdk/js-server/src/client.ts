import { requireServer, serverEnv } from "./runtime.js";
import type { CheckoutParams } from "./checkout-params.js";
import type { Quote, Deposit, EventObjectResponse } from "./types.js";
import {
  ConfigurationError,
  ResponseValidationError,
  AddressMismatchError,
  SignatureVerificationError,
} from "./errors.js";
import { parsePins, validatePins, keyLivemode, normalizeOrigin, type Pins } from "./pins.js";
import { Transport, type RequestOptions } from "./transport.js";
import * as Resources from "./resources.js";
import { constructEvent, type ConstructEventOptions } from "./webhook.js";
import { isRecord, parseJson } from "./json.js";
import { contracts, schemas } from "./schemas.js";
import { valid } from "./validation.js";
import { verifyQuoteAddress, verifyDepositAddress } from "./addresses.js";
export interface PhalaPayOptions {
  apiKey: string;
  pins: string | Pins;
  apiBase?: string;
  timeoutMs?: number;
  maxAttempts?: number;
  requestDeadlineMs?: number;
  /** Retry safe requests through a planned upgrade for up to five minutes; default false. */
  upgradeTolerance?: boolean;
  fetch?: typeof globalThis.fetch;
}
export type Event = Omit<EventObjectResponse, "data" | "pending_webhooks"> & {
  readonly data: Omit<EventObjectResponse["data"], "object" | "previous_attributes"> & {
    readonly object: Readonly<Record<string, unknown>>;
    readonly previous_attributes?: Readonly<Record<string, unknown>> | null;
  };
  readonly pending_webhooks?: EventObjectResponse["pending_webhooks"];
  readonly deposit?: Deposit;
};
export class PhalaPay {
  readonly #pins: Pins;
  readonly #transport: Transport;
  readonly #quotes = new WeakMap<object, string>();
  get pins(): Pins {
    return this.#pins;
  }
  get livemode(): boolean {
    return this.#pins.livemode;
  }
  get apiBase(): string {
    return this.#pins.api_base;
  }
  readonly quotes = new Resources.QuotesResource(this.#execute.bind(this));
  readonly depositAddresses = new Resources.DepositAddressesResource(this.#execute.bind(this));
  readonly deposits = new Resources.DepositsResource(this.#execute.bind(this));
  readonly refunds = new Resources.RefundsResource(this.#execute.bind(this));
  readonly paymentSettings = new Resources.PaymentSettingsResource(this.#execute.bind(this));
  readonly config = new Resources.ConfigResource(this.#execute.bind(this));
  readonly account = new Resources.AccountResource(this.#execute.bind(this));
  readonly treasuries = new Resources.TreasuriesResource(this.#execute.bind(this));
  readonly apiKeys = new Resources.ApiKeysResource(this.#execute.bind(this));
  readonly webhookEndpoints = new Resources.WebhookEndpointsResource(this.#execute.bind(this));
  readonly events = new Resources.EventsResource(this.#execute.bind(this));
  readonly balance = new Resources.BalanceResource(this.#execute.bind(this));
  readonly sweeps = new Resources.SweepsResource(this.#execute.bind(this));
  readonly forwarders = new Resources.ForwardersResource(this.#execute.bind(this));
  readonly webhooks = Object.freeze({ constructEvent: this.#constructEvent.bind(this) });
  constructor(options: PhalaPayOptions) {
    requireServer();
    this.#pins =
      typeof options.pins === "string" ? parsePins(options.pins) : validatePins(options.pins);
    if (keyLivemode(options.apiKey) !== this.livemode)
      throw new ConfigurationError("API key and pins mode differ");
    if (
      options.apiBase !== undefined &&
      normalizeOrigin(options.apiBase, !this.livemode) !== this.apiBase
    )
      throw new ConfigurationError("API origin must match pins");
    if (options.fetch !== undefined && typeof options.fetch !== "function")
      throw new ConfigurationError("Invalid fetch transport");
    this.#transport = new Transport({ ...options, apiBase: this.apiBase });
  }
  static fromEnv(
    env?: Readonly<Record<string, string | undefined>>,
    options: { apiBase?: string } = {},
  ): PhalaPay {
    requireServer();
    const values = env ?? serverEnv();
    const apiKey = values["PHALA_PAY_API_KEY"];
    const pins = values["PHALA_PAY_PINS"];
    if (!apiKey || !pins)
      throw new ConfigurationError("PHALA_PAY_API_KEY and PHALA_PAY_PINS are required");
    return new PhalaPay({
      apiKey,
      pins,
      ...(options.apiBase === undefined ? {} : { apiBase: options.apiBase }),
    });
  }
  close(): Promise<void> {
    return this.#transport.close();
  }
  checkoutParams(quote: Quote): CheckoutParams {
    if (
      this.#quotes.get(quote) !== quote.client_secret ||
      !this.#quotes.has(quote) ||
      quote.status !== "open" ||
      typeof quote.client_secret !== "string" ||
      !quote.client_secret.startsWith(`${quote.id}_secret_`) ||
      quote.expires_at <= Date.now() / 1000
    )
      throw new ResponseValidationError("Quote cannot be used for checkout");
    this.#verify(quote);
    return {
      clientSecret: quote.client_secret,
      expectedAddress: verifyQuoteAddress(this.pins, quote),
      apiBase: this.apiBase,
    };
  }
  async #execute<T>(
    operation: string,
    method: string,
    path: string,
    params: unknown,
    options?: RequestOptions,
  ): Promise<T> {
    const contract = contracts[operation];
    if (!contract) throw new ConfigurationError("Unknown API operation");
    if (contract.body && !valid(params, contract.body))
      throw new ConfigurationError("Invalid request parameters");
    const result = await this.#transport.request(method, path, params, options, contract.response);
    this.#verify(result);
    if (
      operation === "create_quote" &&
      isRecord(result) &&
      typeof result["client_secret"] === "string"
    )
      this.#quotes.set(result, result["client_secret"]);
    return result as T;
  }
  #verify(value: unknown): void {
    if (Array.isArray(value)) {
      value.forEach((item) => this.#verify(item));
      return;
    }
    if (!isRecord(value)) return;
    if (
      Object.hasOwn(value, "account") &&
      typeof value["account"] === "string" &&
      value["account"] !== this.pins.account
    )
      throw new ResponseValidationError("Response account differs from pins");
    if (Object.hasOwn(value, "livemode") && value["livemode"] !== this.livemode)
      throw new ResponseValidationError("Response mode differs from pins");
    if (value["object"] === "account" && value["id"] !== this.pins.account)
      throw new ResponseValidationError("Response account differs from pins");
    if (value["object"] === "quote" && value["status"] === "open") {
      if (!valid(value, schemas["Quote"] ?? {})) throw new ResponseValidationError();
      const quote = value as Quote;
      if (!this.pins.treasuries[String(quote.chain_id)])
        throw new AddressMismatchError("Missing pinned chain");
      verifyQuoteAddress(this.pins, quote);
    }
    if (value["object"] === "deposit_address" && value["status"] === "active") {
      if (!valid(value, schemas["DepositAddress"] ?? {})) throw new ResponseValidationError();
      const address = value as import("./types.js").DepositAddress;
      for (const network of address.networks)
        if (!this.pins.treasuries[String(network.chain_id)])
          throw new AddressMismatchError("Missing pinned chain");
      verifyDepositAddress(this.pins, address);
      if (
        typeof address.address === "string" &&
        address.networks.some(
          (network) => network.address.toLowerCase() !== address.address?.toLowerCase(),
        )
      )
        throw new AddressMismatchError("Deposit address differs from its verified networks");
    }
    // Only resource-bearing fields; metadata and unknown additional fields are opaque merchant data.
    for (const key of ["data", "deposit", "quote", "deposit_address", "object"]) {
      if (isRecord(value[key]) || Array.isArray(value[key])) this.#verify(value[key]);
    }
  }
  async #constructEvent(
    body: string | Uint8Array,
    headers: Headers | Record<string, string | string[] | undefined>,
    options: Pick<ConstructEventOptions, "tolerance"> = {},
  ): Promise<Event> {
    if (
      options.tolerance !== undefined &&
      (!Number.isFinite(options.tolerance) || options.tolerance < 0)
    )
      throw new ConfigurationError("Invalid webhook tolerance");
    try {
      const verified = await constructEvent(
        body,
        headers,
        this.pins.webhook_keys.map((key) => key.public_key),
        {
          ...(options.tolerance === undefined ? {} : { tolerance: options.tolerance }),
          expectedAccount: this.pins.account,
          expectedLivemode: this.livemode,
        },
      );
      // Legacy low-level verification normalizes previous_attributes; bound events preserve the wire value.
      const raw: unknown = parseJson(
        typeof body === "string" ? body : new TextDecoder("utf-8", { fatal: true }).decode(body),
      );
      const schema = schemas["EventObjectResponse"] ?? {};
      if (
        !isRecord(raw) ||
        !valid(raw, {
          ...schema,
          required: schema.required?.filter((field) => field !== "pending_webhooks") ?? [],
        })
      )
        throw new SignatureVerificationError("Invalid webhook envelope");
      const event: Event = { ...verified, data: raw["data"] as Event["data"] };
      const depositTypes = [
        "deposit.credited",
        "deposit.rejected",
        "deposit.refunded",
        "deposit.reversed",
      ];
      if (depositTypes.includes(event.type)) {
        if (
          event.data.object["object"] !== "deposit" ||
          !valid(event.data.object, schemas["Deposit"] ?? {})
        )
          throw new SignatureVerificationError("Invalid webhook deposit");
        this.#verify(event.data.object);
        return { ...event, deposit: event.data.object as Deposit };
      }
      if (["quote.canceled", "quote.expired"].includes(event.type)) {
        if (!valid(event.data.object, schemas["Quote"] ?? {}))
          throw new SignatureVerificationError("Invalid webhook quote");
        this.#verify(event.data.object);
      }
      return event;
    } catch {
      throw new SignatureVerificationError("Webhook verification failed");
    }
  }
}
