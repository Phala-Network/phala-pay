import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { expect, it } from "vitest";

const source = join(import.meta.dirname, "../src");

// The components must work under `style-src 'self'`: all styling is in the static styles.css.
it("sets no inline style and injects no style element", () => {
  const files = readdirSync(source).filter((name) => /\.tsx?$/.test(name));
  expect(files.length).toBeGreaterThan(0);
  for (const file of files) {
    const code = readFileSync(join(source, file), "utf8");
    expect(code, file).not.toMatch(/\bstyle=|<style|\.style\b|dangerouslySetInnerHTML|insertRule|adoptedStyleSheets/);
  }
});
