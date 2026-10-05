import { expectTypeOf, it } from "vitest";
import type { CheckoutParams as ServerCheckoutParams } from "../src/index.js";
import type { CheckoutParams as BrowserCheckoutParams } from "../../js/src/shared/checkout-params.js";

it("keeps the server and browser checkout handoffs mutually assignable", () => {
  expectTypeOf<ServerCheckoutParams>().toEqualTypeOf<BrowserCheckoutParams>();
});
