import assert from "node:assert/strict";
import test from "node:test";
import { splitIdentifier, splitUnits } from "../src/lib/typesetting.ts";

const units = (text: string) => splitUnits(text).filter((_, index) => index % 2 === 1);

await test("hyphenated words, the product's name, and numbers with their units are kept whole", () => {
  assert.deepEqual(units("~7 s on Base; the five-minute scan; free (Apache-2.0)."), ["~7 s", "five-minute", "Apache-2.0"]);
  assert.deepEqual(units("Phala Pay holds no key; @phala/pay-react pays 80 PHA, $1.00 USDC, ~15 min."), ["Phala Pay", "@phala/pay-react", "80 PHA", "$1.00 USDC", "~15 min"]);
});

await test("words, and numbers without a unit, still break", () => {
  assert.deepEqual(units("About 30 seconds on 2 chains, a sweep to self"), []);
  assert.deepEqual(splitUnits("plain text").join(""), "plain text");
});

await test("an identifier or a URL divides after its separators, and only there", () => {
  assert.deepEqual(splitIdentifier("payment_settings_unconfirmed"), ["payment_", "settings_", "unconfirmed"]);
  assert.deepEqual(splitIdentifier("https://pay.phala.com/v1/quotes?id=1"), ["https://", "pay.", "phala.", "com/", "v1/", "quotes?", "id=", "1"]);
});

await test("rendered markdown keeps units and short identifiers whole, and breaks long ones only where they divide", async () => {
  const { renderFragment } = await import("./markdown.ts");
  const html = await renderFragment(
    "A five-minute scan at ~7 s by Phala Pay; set `topup_admin` or `payment_settings_unconfirmed`, an operator-supplement entry.\n\n### `refund_attachment_limit_exceeded`\n",
    "docs/integration.md",
  );
  for (const unit of ["five-minute", "~7 s", "Phala Pay", "topup_admin"]) {
    assert.ok(html.includes(`<span class="unbroken">${unit}</span>`), `${unit} kept whole`);
  }
  // Longer than a narrow column holds: it may break, at its hyphen or after its separators only.
  assert.ok(!html.includes(`<span class="unbroken">operator-supplement</span>`));
  assert.ok(html.includes(`<span class="unbroken">payment_</span><wbr><span class="unbroken">settings_</span><wbr><span class="unbroken">unconfirmed</span>`));
  // In a heading, an identifier divides even when short enough to keep whole elsewhere.
  assert.ok(html.includes(`refund_</span><wbr>`));
});
