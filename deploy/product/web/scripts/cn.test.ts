import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { cn, FONT_SIZES } from "../src/lib/utils.ts";

// The sizes the stylesheet defines (`--text-<name>: …`, not their `--text-<name>--line-height`
// and the like): every one of them, so a size added there and not to FONT_SIZES fails here.
const css = readFileSync(new URL("../src/index.css", import.meta.url), "utf8");
const SIZES = [...css.matchAll(/--text-([a-z0-9]+(?:-[a-z0-9]+)*):/g)].map(([, name]) => name);

await test("class merging knows exactly the sizes src/index.css defines", () => {
  assert.ok(SIZES.length > 0, "no --text-* sizes found in src/index.css");
  assert.deepEqual([...FONT_SIZES].sort(), [...SIZES].sort());
});

await test("the site's font sizes survive a colour beside them", () => {
  for (const size of SIZES) {
    assert.equal(cn(`text-${size} text-muted-foreground`), `text-${size} text-muted-foreground`, size);
  }
});

await test("a later size still replaces an earlier one", () => {
  for (const size of SIZES) {
    assert.equal(cn(`text-sm text-${size}`), `text-${size}`, size);
    assert.equal(cn(`text-${size} text-sm`), "text-sm", size);
  }
});
