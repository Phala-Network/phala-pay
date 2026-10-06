// Approved copy; facts checked against README.md and docs/overview.md, architecture.md, integration.md.
export const TAGLINE = "Crypto payments, without a custodian";
export const HERO_SUBHEAD = "Self-host an open-source payments API. Customers pay in USDC, USDT, or other ERC-20 tokens on Ethereum or Base; deposits can only reach your treasury.";
export const HOME_TITLE = "Phala Pay: self-hosted, non-custodial crypto payments";
export const HOME_DESCRIPTION = "Open-source payments API for ERC-20 tokens on Ethereum and Base. Deposits can only reach your treasury. No per-payment fee.";
export const FAQ = [
  { question: "What does Phala Pay cost?", answer: "The software is free and open source (Apache-2.0) and takes no fee per payment. You pay for your Phala Cloud instance, two RPC providers per chain, backup storage, and the gas when you sweep. Payers pay their own network fees." },
  { question: "Who holds the money?", answer: "No one but you. Each deposit address is a contract that can only pay your treasury, an address or Safe you prove you control. Phala Pay holds no keys to your funds and sends no transactions." },
  { question: "Which chains and tokens are supported?", answer: "ERC-20 tokens on Ethereum and Base: stablecoins such as USDC and USDT, and volatile tokens priced on-chain, such as PHA via TWAP, valued in USD at the quoted price (or, for deposit addresses and mismatched payments, at the price when confirmed). A live (production) route needs commercially licensed price sources. USDC and USDT have commercially licensed price sources; other tokens depend on theirs. The live demo uses test tokens on Sepolia and Base Sepolia." },
  { question: "How fast is a payment credited?", answer: "At the chain's confirmation: about 7 seconds on Base and about 30 seconds on Ethereum. The deposit is then watched to finality (about 15 minutes on Ethereum); if a reorg replaces it, you receive a deposit.reversed event." },
  { question: "How do refunds work?", answer: "You send the refund from your own wallet or Safe and declare it through the API. Phala Pay verifies the transfer on-chain at finality and sends deposit.refunded so your ledger stays correct." },
  { question: "Is there a hosted version?", answer: "No public hosted service. You run your own instance on your own Phala Cloud workspace, with a one-command testnet quick start. Phala runs an instance only for Phala Cloud." },
  { question: "What is the security model?", answer: "The service runs in an Intel TDX confidential VM whose attestation you can verify. Payments are confirmed by two independent RPC providers, and payers are screened against sanctions lists. Phala Pay is pre-1.0 and has not had a third-party security audit." },
  { question: "Do I need KYC or a merchant account?", answer: "Phala Pay has no signup and does not onboard you. You run the instance and decide your own compliance obligations; it screens payers against sanctions lists and leaves KYC to you." },
];

export const HOME_KEYWORDS = "non-custodial crypto payment gateway, self-hosted crypto payments, accept USDC payments API, crypto top-up API";
