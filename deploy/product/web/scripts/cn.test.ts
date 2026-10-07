import assert from "node:assert/strict";
import test from "node:test";
import { cn, FONT_SIZES } from "../src/lib/utils.ts";

await test("the site's font sizes survive a colour beside them", () => {
  assert.equal(cn("text-mono text-foreground"), "text-mono text-foreground");
  for (const size of FONT_SIZES) {
    assert.equal(cn(`text-${size} text-muted-foreground`), `text-${size} text-muted-foreground`, size);
  }
});

await test("a later size still replaces an earlier one", () => {
  for (const size of FONT_SIZES) {
    assert.equal(cn(`text-sm text-${size}`), `text-${size}`, size);
    assert.equal(cn(`text-${size} text-sm`), "text-sm", size);
  }
});
