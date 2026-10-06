import type { ClientQuote } from "../../js/src/index.js";

export const TOKEN = "0x8F40e7E99678F44c88158f049E62817580ab113B";
export const ADDRESS = "0x1111111111111111111111111111111111111111";
export const QUOTE_ID = "qt_0c6e1d0a9b3f4c2e8d7a6b5c4d3e2f10";
export const CLIENT_SECRET = `${QUOTE_ID}_secret_${"ab".repeat(24)}`;
export const API_BASE = "https://topup.example";

export function quote(overrides: Partial<ClientQuote> = {}): ClientQuote {
  const base: ClientQuote = {
    id: QUOTE_ID,
    object: "quote",
    livemode: false,
    status: "open",
    amount: 2500,
    currency: "usd",
    asset: "pha",
    decimals: 18,
    chain_id: 11155111,
    amount_atomic: "100502512562814070352",
    address: ADDRESS,
    payment_uri: "",
    expires_at: 1_790_410_500,
    payment_status: "none",
    confirmations: null,
    amount_credited: null,
    typical_credit_seconds: 30,
  };
  const merged = { ...base, ...overrides };
  return {
    ...merged,
    payment_uri:
      overrides.payment_uri ??
      `ethereum:${TOKEN}@${merged.chain_id}/transfer?address=${merged.address}&uint256=${merged.amount_atomic}`,
  };
}

/** A `fetch` answering each call with the next response, repeating the last one. */
export function fakeFetch(...responses: (ClientQuote | number | Error | (() => Response))[]) {
  const calls: string[] = [];
  let index = 0;
  const fetch = (input: RequestInfo | URL): Promise<Response> => {
    calls.push(typeof input === "string" ? input : input instanceof URL ? input.href : input.url);
    const next = responses[Math.min(index, responses.length - 1)];
    index += 1;
    if (next instanceof Error) {
      return Promise.reject(next);
    }
    if (typeof next === "number") {
      return Promise.resolve(new Response("{}", { status: next }));
    }
    if (typeof next === "function") {
      return Promise.resolve(next());
    }
    return Promise.resolve(Response.json(next));
  };
  return { fetch, calls };
}

/** Matches the element showing `value`, a hex value shown in groups of four characters. */
export function shownValue(value: string) {
  return (_: string, element: Element | null) =>
    element?.classList.contains("pp-value") === true && element.textContent === value;
}
