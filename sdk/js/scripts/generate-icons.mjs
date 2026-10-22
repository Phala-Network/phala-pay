// Vendor only the selected web3icons SVGs. No web3icons runtime dependency is needed.
// Tokens use the `background` variant (one full-bleed brand tile, glyph inset 4px) so every
// coin shares one shape; networks use the `branded` logo as drawn by the project.
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { URL, pathToFileURL } from "node:url";

const version = "4.0.58";
const icons = {
  usdc: "tokens/background/USDC",
  usdt: "tokens/background/USDT",
  pha: "tokens/background/PHA",
  eth: "tokens/background/ETH",
  ethereum: "networks/branded/ethereum",
  base: "networks/branded/base",
};
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
    const [kind, variant, symbol] = source.split("/");
    const file = join(work, "package", "dist", "svgs", kind, variant, `${symbol}.svg.js`);
    let { default: svg } = await import(pathToFileURL(file).href);
    // `background` icons wrap their paths in a clip equal to the 24x24 viewBox. It is a no-op,
    // so unwrap it rather than ship duplicate element ids into host pages.
    const clip = typeof svg === "string" && svg.match(/^(<svg\s[^>]+>)\s*<g clip-path="url\(#([\w-]+)\)">([\s\S]*?)<\/g>\s*<defs>\s*<clipPath id="([\w-]+)">\s*<path fill="#fff" d="M0 0h24v24H0z"\/>\s*<\/clipPath>\s*<\/defs>\s*<\/svg>\s*$/);
    if (clip) {
      if (clip[2] !== clip[4]) throw new Error(`Unexpected clip reference: ${source}`);
      svg = `${clip[1]}${clip[3]}</svg>`;
    }
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
    const body = svg.match(/<svg[^>]+>([\s\S]*)<\/svg>/)[1].trim().replace(/\s*\n\s*/g, "");
    markup.push(`  ${name}: ${JSON.stringify(`<svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" fill="none" viewBox="0 0 24 24" aria-hidden="true">${body}</svg>`)},`);
    jsx.push(`  ${name}: <>${body.replaceAll("fill-rule", "fillRule").replaceAll("clip-rule", "clipRule")}</>,`);
  }
  writeFileSync(new URL("src/icon-svg.ts", root), header + `\nexport const ICON_SVG: Record<${Object.keys(icons).map((key) => JSON.stringify(key)).join(" | ")}, string> = {\n${markup.join("\n")}\n};\n`);
  writeFileSync(new URL("../js-react/src/icon-paths.tsx", root), header + `\nexport const ICON_PATHS = {\n${jsx.join("\n")}\n};\n`);
} finally {
  rmSync(work, { recursive: true, force: true });
}
