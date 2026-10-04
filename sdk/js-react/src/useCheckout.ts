import { useCallback, useEffect, useRef, useState } from "react";
import {
  createCheckout,
  type CheckoutOptions,
  type CheckoutSession,
  type CheckoutState,
} from "@phala/pay";

export type UseCheckoutOptions = Pick<
  CheckoutOptions,
  "clientSecret" | "expectedAddress" | "apiBase" | "pollInterval" | "signal" | "requestTimeout"
>;

export interface UseCheckoutResult extends CheckoutState {
  /** Reads the quote now, for example right after the wallet broadcast the payment. */
  refresh: () => void;
}

const LOADING: CheckoutState = { status: "loading", quote: null, error: null };

/** Follows a quote's public view for as long as the component is mounted. */
export function useCheckout({
  clientSecret,
  expectedAddress,
  apiBase,
  pollInterval,
  signal,
  requestTimeout,
}: UseCheckoutOptions): UseCheckoutResult {
  const [current, setCurrent] = useState({ key: "", state: LOADING });
  const checkout = useRef<CheckoutSession | null>(null);
  const key = `${apiBase} ${clientSecret} ${expectedAddress}`;

  useEffect(() => {
    const instance = createCheckout({
      clientSecret,
      expectedAddress,
      apiBase,
      ...(pollInterval === undefined ? {} : { pollInterval }),
      ...(signal === undefined ? {} : { signal }),
      ...(requestTimeout === undefined ? {} : { requestTimeout }),
    });
    checkout.current = instance;
    const unsubscribe = instance.subscribe((state) => setCurrent({ key, state }));
    return () => {
      unsubscribe();
      instance.destroy();
      checkout.current = null;
    };
  }, [key, clientSecret, expectedAddress, apiBase, pollInterval, signal, requestTimeout]);

  const refresh = useCallback(() => {
    void checkout.current?.refresh();
  }, []);

  // A new client secret starts from `loading`, not from the previous quote's state.
  return { ...(current.key === key ? current.state : LOADING), refresh };
}
