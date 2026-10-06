// Access date for all sources: 2026-10-05
export type CellStatus = "verified" | "partially" | "not-stated";
export interface Cell { text: string; source: string; status: CellStatus }
export interface Competitor {
  id: string; name: string;
  custody: Cell; selfHosted: Cell; verifiability: Cell; fees: Cell; chains: Cell;
  speed: Cell; refunds: Cell; api: Cell; onboarding: Cell; payoutToOwnWallet: Cell;
}
export const COMPARE_ACCESSED = "2026-10-05";

export const competitors: Competitor[] = [
  {
    id: "stripe", name: "Stripe stablecoin payments",
    custody: { text: "Settles to your Stripe balance in local currency", source: "https://docs.stripe.com/crypto/stablecoin-payments", status: "verified" },
    selfHosted: { text: "No (hosted service)", source: "https://docs.stripe.com/crypto/stablecoin-payments", status: "verified" },
    verifiability: { text: "N/A (hosted service)", source: "https://docs.stripe.com/crypto/stablecoin-payments", status: "verified" },
    fees: { text: "1.5% per transaction (US pricing page)", source: "https://stripe.com/pricing", status: "verified" },
    chains: { text: "USDC (Tempo, Ethereum, Solana, Polygon, Base); USDP, USDG US-only", source: "https://docs.stripe.com/crypto/stablecoin-payments", status: "verified" },
    speed: { text: "Payout timing \"varies by network\"", source: "https://docs.stripe.com/crypto/stablecoin-payments", status: "verified" },
    refunds: { text: "Full and partial; returned as stablecoins to original wallet", source: "https://docs.stripe.com/crypto/stablecoin-payments", status: "verified" },
    api: { text: "Stripe API, Checkout, Elements, Billing, Connect", source: "https://docs.stripe.com/crypto/stablecoin-payments", status: "verified" },
    onboarding: { text: "Stripe account; US (EU, HK, MX, CH in private preview)", source: "https://docs.stripe.com/crypto/stablecoin-payments", status: "verified" },
    payoutToOwnWallet: { text: "No; settles in fiat to your Stripe balance", source: "https://docs.stripe.com/crypto/stablecoin-payments", status: "verified" },
  },
  {
    id: "coinbase-business", name: "Coinbase Business (Checkouts API)",
    custody: { text: "Custodial; funds credited to your Coinbase Business account", source: "https://help.coinbase.com/en/transitioning-from-coinbase-commerce-to-coinbase-business", status: "verified" },
    selfHosted: { text: "No (hosted service)", source: "https://docs.cdp.coinbase.com/coinbase-business/checkout-apis/overview", status: "verified" },
    verifiability: { text: "N/A (hosted service)", source: "https://docs.cdp.coinbase.com/coinbase-business/checkout-apis/overview", status: "verified" },
    fees: { text: "Per-payment fee; current rate shown in app (1% announced Oct 2025)", source: "https://docs.cdp.coinbase.com/coinbase-business/checkout-apis/overview", status: "partially" },
    chains: { text: "USDC on Base; more networks planned", source: "https://docs.cdp.coinbase.com/coinbase-business/checkout-apis/migrate-from-commerce/faq", status: "verified" },
    speed: { text: "\"Settle in under a second on Base\" (payment links)", source: "https://www.coinbase.com/blog/introducing-a-powerful-suite-of-business-payment-tools-on-coinbase-business", status: "partially" },
    refunds: { text: "Full or partial refunds via API, paid in USDC", source: "https://docs.cdp.coinbase.com/api-reference/business-api/rest-api/checkouts-spec.yaml", status: "verified" },
    api: { text: "REST Checkouts and Payment Links APIs, webhooks, JWT auth", source: "https://docs.cdp.coinbase.com/coinbase-business/checkout-apis/overview", status: "verified" },
    onboarding: { text: "Business account; registered entity in US or Singapore", source: "https://help.coinbase.com/en/transitioning-from-coinbase-commerce-to-coinbase-business", status: "verified" },
    payoutToOwnWallet: { text: "Withdraw to bank; USDC payouts to onchain addresses", source: "https://www.coinbase.com/blog/introducing-a-powerful-suite-of-business-payment-tools-on-coinbase-business", status: "partially" },
  },
  {
    id: "btcpay", name: "BTCPay Server",
    custody: { text: "Non-custodial; \"payments go directly to your wallet\"", source: "https://docs.btcpayserver.org/FAQ/General/", status: "verified" },
    selfHosted: { text: "Yes, self-hosted; MIT license", source: "https://docs.btcpayserver.org/FAQ/General/", status: "verified" },
    verifiability: { text: "Open source; you run the code (no TEE attestation)", source: "https://docs.btcpayserver.org/FAQ/General/", status: "verified" },
    fees: { text: "No transaction fees", source: "https://docs.btcpayserver.org/FAQ/General/", status: "verified" },
    chains: { text: "Bitcoin, Lightning; community altcoins incl. USDt (Tron, Liquid) via plugins", source: "https://docs.btcpayserver.org/FAQ/Altcoin/", status: "verified" },
    speed: { text: "Merchant-set confirmations; Lightning settles instantly", source: "https://docs.btcpayserver.org/FAQ/Stores/", status: "verified" },
    refunds: { text: "Built-in refunds via pull payments; merchant signs payout", source: "https://docs.btcpayserver.org/Refund/", status: "verified" },
    api: { text: "Greenfield REST API; many e-commerce plugins", source: "https://docs.btcpayserver.org/API/Greenfield/v1/", status: "verified" },
    onboarding: { text: "None; self-run, no third party", source: "https://docs.btcpayserver.org/FAQ/General/", status: "verified" },
    payoutToOwnWallet: { text: "Yes, directly to your wallet", source: "https://docs.btcpayserver.org/FAQ/General/", status: "verified" },
  },
  {
    id: "nowpayments", name: "NOWPayments",
    custody: { text: "Direct to merchant wallet by default; optional custody", source: "https://nowpayments.io/blog/how-secure-is-the-nowpayments-custodial-solution", status: "verified" },
    selfHosted: { text: "No (hosted service)", source: "https://nowpayments.io/api", status: "verified" },
    verifiability: { text: "N/A (hosted service)", source: "https://nowpayments.io/api", status: "verified" },
    fees: { text: "1% service fee; auto-conversion adds a 1% fee", source: "https://nowpayments.io/pricing", status: "partially" },
    chains: { text: "350+ currencies", source: "https://nowpayments.io/pricing", status: "verified" },
    speed: { text: "\"5 min\" average transaction time", source: "https://nowpayments.io/pricing", status: "verified" },
    refunds: { text: "Merchant sends refund from own wallet", source: "https://nowpayments.io/help/payments/common/refund-policy", status: "verified" },
    api: { text: "API with IPN callbacks, sandbox, e-commerce plugins", source: "https://nowpayments.io/api", status: "verified" },
    onboarding: { text: "KYC/AML checks may be requested; vendor says \"extremely rare\"", source: "https://nowpayments.io/help/security/kyc-aml/why-the-kyc-aml", status: "partially" },
    payoutToOwnWallet: { text: "Yes in default mode; funds sent to your wallet", source: "https://nowpayments.io/help/payments/common/refund-policy", status: "verified" },
  },
  {
    id: "moonpay-commerce", name: "MoonPay Commerce (formerly Helio)",
    custody: { text: "\"Does not custody funds\"; payments go to merchant wallets", source: "https://docs.hel.io/docs/faq", status: "verified" },
    selfHosted: { text: "No (hosted service)", source: "https://docs.hel.io/llms.txt", status: "verified" },
    verifiability: { text: "N/A (hosted service)", source: "https://docs.hel.io/llms.txt", status: "verified" },
    fees: { text: "2% standard; 1% with HelioX; custom high-volume pricing", source: "https://docs.hel.io/docs/pricing-fees", status: "verified" },
    chains: { text: "USDC, SOL, ETH, BTC and more; Solana, EVM, Bitcoin", source: "https://docs.hel.io/docs/faq", status: "verified" },
    speed: { text: "Vendor states merchants are paid \"instantly\"", source: "https://docs.hel.io/docs/faq", status: "verified" },
    refunds: { text: "Merchant issues refunds via dashboard", source: "https://docs.hel.io/docs/verification", status: "partially" },
    api: { text: "REST API, @heliofi SDKs, signed webhooks, checkout widget", source: "https://docs.hel.io/llms.txt", status: "verified" },
    onboarding: { text: "Self-serve; KYC/KYB merchant verification via Sumsub", source: "https://docs.hel.io/docs/verification", status: "partially" },
    payoutToOwnWallet: { text: "Yes, paid directly to merchant wallets", source: "https://docs.hel.io/docs/faq", status: "verified" },
  },
];

// Phala Pay facts: README.md; docs/overview.md; docs/architecture.md §§1, 8; docs/integration.md §§1.6, 3, 5.
const repo = "https://github.com/Phala-Network/phala-pay";
const overview = `${repo}/blob/main/docs/overview.md`;
const architecture = `${repo}/blob/main/docs/architecture.md`;
const integration = `${repo}/blob/main/docs/integration.md`;
const readme = `${repo}/blob/main/README.md`;
export const phalaPay: Competitor = {
  id: "phala-pay", name: "Phala Pay",
  custody: { text: "Non-custodial: contracts that can only pay your treasury", source: overview, status: "verified" },
  selfHosted: { text: "Yes, Apache-2.0, on your Phala Cloud workspace", source: readme, status: "verified" },
  verifiability: { text: "Intel TDX attestation; webhook key pinned from it", source: integration, status: "verified" },
  fees: { text: "No per-payment fee; you pay hosting, RPC, sweep gas", source: architecture, status: "verified" },
  chains: { text: "ERC-20 tokens on Ethereum and Base, including USDC and USDT", source: architecture, status: "verified" },
  speed: { text: "About 7 s (Base) / 30 s (Ethereum), then watched to finality", source: overview, status: "verified" },
  refunds: { text: "From your wallet or Safe; verified on-chain; webhook updates ledger", source: integration, status: "verified" },
  api: { text: "Stripe-style REST; JS (browser, React, server), Python", source: readme, status: "verified" },
  onboarding: { text: "No signup; operator onboards accounts and owns compliance; payer sanctions screening built in", source: overview, status: "verified" },
  payoutToOwnWallet: { text: "Yes, the only possible destination; you sweep", source: integration, status: "verified" },
};

export const dimensions: { key: keyof Omit<Competitor, "id" | "name">; label: string }[] = [
  { key: "custody", label: "Custody" },
  { key: "selfHosted", label: "Self-hosting / open source" },
  { key: "verifiability", label: "Verifiability" },
  { key: "fees", label: "Fees" },
  { key: "chains", label: "Chains / tokens" },
  { key: "speed", label: "Credit / settlement speed" },
  { key: "refunds", label: "Refunds" },
  { key: "api", label: "API / SDK" },
  { key: "onboarding", label: "KYC / merchant onboarding" },
  { key: "payoutToOwnWallet", label: "Payout to your own wallet" },
];

const archived = new Set([
  "https://help.coinbase.com/en/transitioning-from-coinbase-commerce-to-coinbase-business",
  "https://www.coinbase.com/blog/introducing-a-powerful-suite-of-business-payment-tools-on-coinbase-business",
]);
export const sources = [...new Set([phalaPay, ...competitors].flatMap((vendor) => dimensions.map(({ key }) => vendor[key].source)))].map((url) => ({ url, archived: archived.has(url) }));
export const COMPARE_TITLE = "Crypto payment gateways compared | Phala Pay";
export const COMPARE_DESCRIPTION = "How Phala Pay compares with Stripe, Coinbase Business, BTCPay Server, NOWPayments, and MoonPay Commerce on custody, fees, chains, speed, and refunds.";

export const COMPARE_KEYWORDS = "crypto payment gateway comparison, self-hosted crypto payments, Stripe stablecoin payments, Coinbase Business, BTCPay Server, NOWPayments, MoonPay Commerce";
