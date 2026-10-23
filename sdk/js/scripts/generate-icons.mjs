// Vendor CoinMarketCap's token and network logos as PNG data URIs, so the SDK makes no runtime
// image requests. Each network is the CoinMarketCap asset that CoinMarketCap itself shows for the
// chain (its `platformCryptoId`): Ethereum 1027, Base 27716. Testnets reuse these (src/chains.ts).
// The 64px images stay sharp at the 16 to 24px icon sizes on 2x displays. Each download
// must match its pinned SHA-256: when one changes upstream, review the new image, then update its pin.
/* global fetch, AbortSignal */
import { Buffer } from "node:buffer";
import { createHash } from "node:crypto";
import { writeFileSync } from "node:fs";
import { URL } from "node:url";

const size = 64;
const icons = {
  usdc: { id: 3408, sha256: "6f2390c4bf94ba36584793958432d5069f2bec31dfa7cb9b93c0c76cf965a901" },
  usdt: { id: 825, sha256: "2d5a29e2c8a731b9c0891dfa54c3bf753ad5bba2911e71ebca654c273cf08748" },
  pha: { id: 6841, sha256: "52fda701ccfd0644fb934c717df8ae139aac8ebc71f59fe78924f5b0e42fe5de" },
  eth: { id: 1027, sha256: "eebe29898b8b7de5c9e47daab474152be8095e3ab42d768b84b085c5a12b95c6" },
  ethereum: { id: 1027, sha256: "eebe29898b8b7de5c9e47daab474152be8095e3ab42d768b84b085c5a12b95c6" },
  base: { id: 27716, sha256: "ab304d91e519a4986184b4d3c5931ee9fc1da404d52163ed4292ac89ddaf4957" },
};
const root = new URL("../", import.meta.url);
const header = `/** Generated from CoinMarketCap's ${size}x${size} logos by scripts/generate-icons.mjs. Do not edit.
 * The logos are trademarks of their owners; see THIRD_PARTY_NOTICES.
 */
`;
const PNG = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

async function download(url) {
  let response;
  try {
    response = await fetch(url, { signal: AbortSignal.timeout(30_000) });
  } catch (cause) {
    throw new Error(`Could not download ${url}`, { cause });
  }
  if (!response.ok) throw new Error(`Could not download ${url}: HTTP ${response.status}`);
  return Buffer.from(await response.arrayBuffer());
}

const images = [];
for (const [name, { id, sha256 }] of Object.entries(icons)) {
  const url = `https://s2.coinmarketcap.com/static/img/coins/${size}x${size}/${id}.png`;
  const bytes = await download(url);
  const actual = createHash("sha256").update(bytes).digest("hex");
  if (actual !== sha256) throw new Error(`${url} changed: SHA-256 ${actual}, pinned ${sha256}`);
  // A PNG signature and an IHDR chunk of the expected size; the pin already fixes the content.
  if (!bytes.subarray(0, 8).equals(PNG) || bytes.toString("latin1", 12, 16) !== "IHDR"
    || bytes.readUInt32BE(16) !== size || bytes.readUInt32BE(20) !== size) {
    throw new Error(`${url} is not a ${size}x${size} PNG`);
  }
  images.push(`  ${name}: "data:image/png;base64,${bytes.toString("base64")}",`);
}
const output = header + `\nexport const ICON_IMAGES: Record<${Object.keys(icons).map((key) => JSON.stringify(key)).join(" | ")}, string> = {\n${images.join("\n")}\n};\n`;
writeFileSync(new URL("src/icon-images.ts", root), output);
writeFileSync(new URL("../js-react/src/icon-images.ts", root), output);
