// The hero's integration, as sdk/js-server/README.md and docs/integration.md#quickstart write it: the
// server creates a quote (amount in US cents) and returns only its checkout params to the signed-in
// browser, which spreads them into <Checkout>; the account is credited from the signed webhook. The
// code is src/snippets/, compiled by the typecheck against the SDK packages and shown as written.
// Only Vite loads this module (`?raw`); the copy that Node scripts read stays in site.ts.
import serverSnippet from "../snippets/top-up.ts?raw";
import pageSnippet from "../snippets/TopUp.tsx?raw";

export const HERO_CODE = [
  { label: "Your server", file: "top-up.ts", code: serverSnippet.trimEnd() },
  { label: "Your page", file: "TopUp.tsx", code: pageSnippet.trimEnd() },
];
export const HERO_CODE_NOTE = { before: "Credit the account when the signed ", code: "deposit.credited", after: " webhook arrives." };
