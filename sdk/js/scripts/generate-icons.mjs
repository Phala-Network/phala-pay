// Vendor only the selected branded SVGs. No web3icons runtime dependency is needed.
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { URL, pathToFileURL } from "node:url";

const version = "4.0.57";
const icons = { usdc: "tokens/USDC", usdt: "tokens/USDT", pha: "tokens/PHA", eth: "tokens/ETH", ethereum: "networks/ethereum", base: "networks/base" };
const root = new URL("../", import.meta.url);
const header = `/** Generated from @web3icons/core@${version} by scripts/generate-icons.mjs.
 * SPDX-License-Identifier: MIT; Copyright (c) 2024 0xa3k5.
 * See THIRD_PARTY_NOTICES for the full license. Do not edit.
 */
`;
const work = mkdtempSync(join(tmpdir(), "phala-pay-icons-"));
try {
  const archive = execFileSync("npm", ["pack", `@web3icons/core@${version}`, "--pack-destination", work, "--silent"], { encoding: "utf8" }).trim();
  execFileSync("tar", ["-xzf", join(work, archive), "-C", work]);
  const markup = [];
  const jsx = [];
  for (const [name, source] of Object.entries(icons)) {
    const [kind, symbol] = source.split("/");
    const file = join(work, "package", "dist", "svgs", kind, "branded", `${symbol}.svg.js`);
    const { default: svg } = await import(pathToFileURL(file).href);
    // Strict allowlist: these assets need only an SVG root and self-closing paths.
    // Reject styles, scripts, handlers, references, entities and unexpected syntax.
    if (typeof svg !== "string" || !/^<svg\s[^>]+>\s*(?:<path\s[^>]+\/>\s*)+<\/svg>\s*$/.test(svg)) {
      throw new Error(`Unexpected SVG structure: ${source}`);
    }
    for (const tag of svg.matchAll(/<(svg|path)\s([^>]+)>/g)) {
      const allowed = tag[1] === "svg" ? ["xmlns", "width", "height", "fill", "viewBox", "class"] : ["fill", "d", "fill-rule", "clip-rule"];
      let rest = tag[2].replace(/\/$/, "");
      for (const attr of rest.matchAll(/([\w-]+)="([^"]*)"/g)) {
        if (!allowed.includes(attr[1]) || /[<>&]|url\s*\(/i.test(attr[2])) throw new Error(`Unsafe SVG attribute: ${source}`);
        if (attr[1] === "xmlns" && attr[2] !== "http://www.w3.org/2000/svg") throw new Error("Unexpected SVG namespace");
      }
      rest = rest.replace(/[\w-]+="[^"]*"/g, "").trim();
      if (rest !== "") throw new Error(`Unexpected SVG attributes: ${source}`);
    }
    let body = svg.match(/<svg[^>]+>([\s\S]*)<\/svg>/)[1].trim().replace(/\s*\n\s*/g, "");
    // PHA's light brand color needs a neutral backing on light host surfaces.
    if (name === "pha") body = '<circle cx="12" cy="12" r="12" fill="#252525"/>' + body;
    markup.push(`  ${name}: ${JSON.stringify(`<svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" fill="none" viewBox="0 0 24 24" aria-hidden="true">${body}</svg>`)},`);
    jsx.push(`  ${name}: <>${body.replaceAll("fill-rule", "fillRule").replaceAll("clip-rule", "clipRule")}</>,`);
  }
  writeFileSync(new URL("src/icon-svg.ts", root), header + `\nexport const ICON_SVG: Record<${Object.keys(icons).map((key) => JSON.stringify(key)).join(" | ")}, string> = {\n${markup.join("\n")}\n};\n`);
  writeFileSync(new URL("src/react/icon-paths.tsx", root), header + `\nexport const ICON_PATHS = {\n${jsx.join("\n")}\n};\n`);
} finally {
  rmSync(work, { recursive: true, force: true });
}
