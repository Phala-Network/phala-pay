// The hero's integration, as sdk/js-server/README.md and docs/integration.md#quickstart write it: the
// server creates a quote (amount in US cents) and returns only its checkout params to the signed-in
// browser, which spreads them into <Checkout>; the account is credited from the signed webhook. The
// code is src/snippets/, compiled by the typecheck against the SDK packages and shown as written,
// highlighted at build time. Only Vite loads this module (`?highlight`); the copy that Node scripts
// read stays in site.ts.
import server from "../snippets/top-up.ts?highlight";
import page from "../snippets/TopUp.tsx?highlight";

export const HERO_CODE = [
  { id: "server", label: "Server", file: "top-up.ts", ...server },
  { id: "page", label: "Page", file: "TopUp.tsx", ...page },
];
export const HERO_CODE_NOTE = { before: "Credit the account when the signed ", code: "deposit.credited", after: " webhook arrives." };
