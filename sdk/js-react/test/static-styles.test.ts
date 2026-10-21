import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { expect, it } from "vitest";

const source = join(import.meta.dirname, "../src");

// The components must work under `style-src 'self'`: all styling is in the static styles.css. The
// one exception is the open listbox's place beside its field (Select.tsx), set through the CSSOM
// (`element.style.setProperty`), which that policy allows, as it does not a style attribute in markup.
it("sets no inline style and injects no style element", () => {
  const files = readdirSync(source).filter((name) => /\.tsx?$/.test(name));
  expect(files.length).toBeGreaterThan(0);
  for (const file of files) {
    const code = readFileSync(join(source, file), "utf8");
    const allowed = file === "Select.tsx" ? code.replaceAll(/\bbox\.style\.setProperty\(/g, "") : code;
    expect(allowed, file).not.toMatch(/\bstyle=|<style|\.style\b|dangerouslySetInnerHTML|insertRule|adoptedStyleSheets/);
  }
});
