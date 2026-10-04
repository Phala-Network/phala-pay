// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
describe("server entry exclusion", () => {
  it("rejects browser import before a merchant key can be read", async () => {
    await expect(import("../src/index.js")).rejects.toThrow("@phala/pay-server is server-only; never ship a key to the browser");
  });
  it("keeps offline helpers available without a merchant client", async () => {
    const helpers = await import("../src/helpers.js");
    expect(helpers.quoteSalt("acct_example", "customer", "qt_example")).toMatch(/^0x[0-9a-f]{64}$/);
  });
});
