/** The only server-to-browser checkout handoff; contains no merchant credentials or pins. */
export interface CheckoutParams {
  clientSecret: string;
  expectedAddress: string;
  apiBase: string;
}
