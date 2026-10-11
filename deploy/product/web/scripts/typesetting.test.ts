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
