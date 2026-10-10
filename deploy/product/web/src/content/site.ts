// Approved copy; facts checked against README.md and docs/overview.md, architecture.md, integration.md.
export const TAGLINE = "Crypto payments, without a custodian";
export const HERO_SUBHEAD = "Self-host an open-source payments API. Customers pay in USDC, USDT, or other ERC-20 tokens on Ethereum or Base; deposits can only reach your treasury.";
// README.md: pre-1.0, no third-party security audit.
export const HERO_META = "Pre-1.0 · Not yet audited";
// Three facts under the hero's actions. The time is the hinted path's (README.md, "Fast credit";
// docs/integration.md, `typical_credit_seconds`): a payment from the wallet checkout, whose
// transaction hash reaches the service; a manual transfer waits for the five-minute scan.
export const HERO_FACTS = [
  { value: "~7 s", label: "to credit on Base (wallet checkout)" },
  { value: "0%", label: "fee per payment" },
  { value: "Apache-2.0", label: "open source" },
];
export const HOME_TITLE = "Phala Pay: self-hosted, non-custodial crypto payments";
export const NOT_FOUND_TITLE = "Page not found | Phala Pay";
export const HOME_DESCRIPTION = "Open-source payments API for ERC-20 tokens on Ethereum and Base. Deposits can only reach your treasury. No per-payment fee.";

export const DEMO_TITLE = "Try it on testnet";
export const DEMO_STATUS = "Live on Sepolia and Base Sepolia";
export const DEMO_LEAD = "Pay with test tokens and watch the backend follow the payment.";

// Each section's label above its heading.
export const EYEBROWS = { demo: "Live demo", custody: "Custody", features: "Why Phala Pay", compare: "Compare", faq: "FAQ" };
export const FEATURES_TITLE = "For platforms that sell credits";
export const PROPERTIES_LEAD = "Top-ups and credits for apps and platforms, such as AI APIs, cloud, and compute.";
// Where a payment goes, as docs/overview.md and docs/integration.md §1.6 describe it: the headline
// of the section that follows the demo.
export const MONEY_TITLE = "Where the money goes";
export const CUSTODY_PATH = [
  { role: "Payer", name: "Your customer's wallet", detail: "Pays the quote, or any amount to their deposit address." },
  { role: "Contract", name: "A deposit address", detail: "A contract whose only destination is fixed in its address." },
  { role: "You", name: "Your treasury", detail: "An address or Safe you prove you control with a signed message." },
];
export const CUSTODY_LINKS = ["pays", "sweeps only to"];
export const CUSTODY_NOTE = "Phala Pay holds no key to the funds and sends no transactions; the operator cannot change your treasury.";

// Facts: README.md; docs/overview.md; docs/architecture.md §§1, 8; docs/integration.md §§1.6, 5.
export const PROPERTIES = [
  { title: "No custodian", text: "Deposit addresses are contracts that can only pay your treasury. You set the treasury with a signed message; the operator cannot change it." },
  { title: "No per-payment fee", text: "The software takes no cut. You pay for your own hosting, RPC providers, and the gas to sweep." },
  { title: "Credited in seconds", text: "About 7 s on Base and 30 s on Ethereum after payment, double-checked by a second RPC provider and watched to finality." },
  { title: "Verify what runs", text: "It runs in an Intel TDX confidential VM. Check its attestation and pin its webhook signing key from it." },
  { title: "Stripe-style API", text: "Quotes, deposit addresses, test and live modes, idempotency keys, signed webhooks. SDKs for React, Node.js, and Python." },
];

// What each feature card shows above its text, from the same sources as PROPERTIES.
export const FEE_FIGURE = { value: "0%", label: "Taken by the software, per payment" };
export const CREDIT_TIMES = {
  caption: "Wallet checkout: time to credit after paying",
  rows: [{ chain: "Base", value: "~7 s" }, { chain: "Ethereum", value: "~30 s" }],
  note: "A manual transfer is found by the five-minute scan.",
};
export const SDK_PACKAGES = ["@phala/pay-react", "@phala/pay-server", "phala-pay"];
export const WEBHOOK_EVENTS = ["deposit.credited", "deposit.reversed", "deposit.refunded"];
export const VERIFY_CHECKS = ["Intel TDX confidential VM", "Attestation you can check", "Webhook signing key pinned from it"];

export const CLOSING_TITLE = "Run your own payment rail";
export const CLOSING_LEAD = "Deploy a testnet instance to your Phala Cloud workspace with one command.";

export const FAQ = [
  { question: "What does Phala Pay cost?", answer: "The software is free and open source (Apache-2.0) and takes no fee per payment. You pay for your Phala Cloud instance, two RPC providers per chain, backup storage, and the gas when you sweep. Payers pay their own network fees." },
  { question: "Who holds the money?", answer: "No one but you. Each deposit address is a contract that can only pay your treasury, an address or Safe you prove you control. Phala Pay holds no keys to your funds and sends no transactions." },
  { question: "Which chains and tokens are supported?", answer: "ERC-20 tokens on Ethereum and Base: stablecoins such as USDC and USDT, and volatile tokens priced on-chain, such as PHA via TWAP, valued in USD at the quoted price (or, for deposit addresses and mismatched payments, at the price when confirmed). A live (production) route needs commercially licensed price sources. USDC and USDT have commercially licensed price sources; other tokens depend on theirs. The live demo uses test tokens on Sepolia and Base Sepolia." },
  { question: "How fast is a payment credited?", answer: "At the chain's confirmation: about 7 seconds on Base and about 30 seconds on Ethereum. The deposit is then watched to finality (about 15 minutes on Ethereum); if a reorg replaces it, you receive a deposit.reversed event." },
  { question: "How do refunds work?", answer: "You send the refund from your own wallet or Safe and declare it through the API. Phala Pay verifies the transfer on-chain at finality and sends deposit.refunded so your ledger stays correct." },
  { question: "Is there a hosted version?", answer: "No public hosted service. You run your own instance on your own Phala Cloud workspace, with a one-command testnet quick start. Phala runs an instance only for Phala Cloud." },
  { question: "What is the security model?", answer: "The service runs in an Intel TDX confidential VM whose attestation you can verify. Payments are confirmed by two independent RPC providers, and payers are screened against verified OFAC SDN snapshots and operator supplements. Phala Pay is pre-1.0 and has not had a third-party security audit." },
  { question: "Do I need KYC or a merchant account?", answer: "Phala Pay has no signup and does not onboard you. You run the instance and decide your own compliance obligations; it screens payers against verified OFAC SDN snapshots and operator supplements and leaves KYC to you." },
];

export const HOME_KEYWORDS = "non-custodial crypto payment gateway, self-hosted crypto payments, accept USDC payments API, crypto top-up API";

// Update when landing copy changes; independent of checkout history and build time.
export const SITE_UPDATED = "2026-10-06";

// docs/self-hosting.md, "One-command deploy": the latest release, through public/_redirects.
export const DEPLOY_COMMAND = "curl -fsSL https://pay.phala.com/deploy.sh | bash";
