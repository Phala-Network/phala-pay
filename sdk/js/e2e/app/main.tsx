import { StrictMode, useState } from "react";
import { createRoot } from "react-dom/client";
import { createWalletClient, custom, isAddress } from "viem";
import { Checkout, DepositAddress, PaymentElement, PaymentOptionPicker } from "../../../js-react/dist/index.js";
import type { EthereumProvider } from "../../dist/index.js";

const params = new URLSearchParams(window.location.search);
// `?wallet_client=<account>` passes the test wallet as the page's own viem client, as wagmi would.
const account = params.get("wallet_client");
const testWallet = (window as { testWallet?: EthereumProvider }).testWallet;
const walletClient =
  account !== null && isAddress(account) && testWallet !== undefined
    ? createWalletClient({ account, transport: custom(testWallet) })
    : undefined;

const appearance = { theme: params.get("theme") === "dark" ? "dark" as const : "light" as const };

const address = "0x1111111111111111111111111111111111111111";
const token = "0x2222222222222222222222222222222222222222";
// `?networks=1` shows a single network.
const chains = params.get("networks") === "1" ? [11155111] : [11155111, 84532];
const networks = chains.map((chain_id) => ({
  chain_id,
  address,
  assets: ["pha", "usdc"].map((asset) => ({
    asset,
    contract: token,
    decimals: 18,
    min_amount: 500,
    min_deposit_atomic: asset === "usdc" ? "0" : "100000000000000000",
    max_deposit_atomic: "10100000000000000000000",
    payment_uri: `ethereum:${token}@${chain_id}/transfer?address=${address}`,
  })),
}));

// `?picker` shows the payment option picker alone, with a detail on each option. Each carries
// its per-transfer limits, as a `GET /v1/config` asset does.
const stablecoin = { decimals: 6, min_amount: 500, min_deposit_atomic: "0", max_deposit_atomic: "10100000000" };
const options = [
  { chain_id: 1, asset: "usdc", detail: "$1.00", ...stablecoin },
  { chain_id: 1, asset: "usdt", detail: "$1.00", ...stablecoin },
  { chain_id: 8453, asset: "usdc", detail: "$1.00", ...stablecoin },
  {
    chain_id: 11155111,
    asset: "pha",
    detail: "Market rate · +10% bonus",
    decimals: 18,
    min_amount: 500,
    min_deposit_atomic: "100000000000000000",
    max_deposit_atomic: "10100000000000000000000",
  },
  { chain_id: 1, asset: "eth", detail: "Market rate", decimals: 18, min_amount: 500, min_deposit_atomic: "0", max_deposit_atomic: "5000000000000000000" },
];

function Picker() {
  const [option, setOption] = useState(options[2]);
  return (
    <PaymentOptionPicker
      options={options}
      value={option}
      onChange={setOption}
      renderDetail={(each) => each.detail}
      appearance={appearance}
    />
  );
}

// `?element` shows the payment element, whose quote is the page's `client_secret`, and whose
// deposit address is the page's. `?host` hides its submit for the page's summary and button.
function Element() {
  const host = params.has("host");
  return (
    <PaymentElement
      hideSubmit={host}
      options={options}
      defaultValue={options[2]}
      amount={2500}
      renderDetail={(each) => each.detail}
      createQuote={() => Promise.resolve({
        clientSecret: params.get("client_secret") ?? "",
        expectedAddress: params.get("expected_address") ?? "",
        apiBase: params.get("api_base") ?? "",
      })}
      createDepositAddress={() => Promise.resolve({ address, networks })}
      appearance={appearance}
    >
      {host && (
        <>
          <dl className="host-summary">
            <dt>Total</dt>
            <dd>$25.00</dd>
          </dl>
          <button type="submit" className="host-button">Continue to pay $25.00</button>
        </>
      )}
    </PaymentElement>
  );
}

// `?element&deposit_mode` shows the payment element without quotes: submit shows the customer's
// deposit address (`to`, else the page's), which asks for 12.5 of the token (`token`, else the
// page's) on Sepolia or 25 USDC on Base Sepolia, in the units of the address's token (18 and 6
// decimals), and follows its payments with the page's `client_secret`.
const depositOptions = [
  { chain_id: 11155111, asset: "pha" },
  { chain_id: 84532, asset: "usdc" },
];
function DepositMode({ onEvent }: { onEvent: (event: string) => void }) {
  const to = params.get("to") ?? address;
  const contract = params.get("token") ?? token;
  const depositNetworks = depositOptions.map(({ chain_id, asset }) => ({
    chain_id,
    address: to,
    assets: [{
      asset, contract, decimals: asset === "usdc" ? 6 : 18,
      payment_uri: `ethereum:${contract}@${chain_id}/transfer?address=${to}`,
    }],
  }));
  return (
    <PaymentElement
      options={depositOptions}
      defaultValue={depositOptions[params.get("option") === "usdc" ? 1 : 0]}
      amount={2500}
      createDepositAddress={() =>
        Promise.resolve({
          address: to,
          networks: depositNetworks,
          ...(params.has("client_secret") ? { clientSecret: params.get("client_secret") ?? "" } : {}),
          ...(params.has("api_base") ? { apiBase: params.get("api_base") ?? "" } : {}),
        })
      }
      depositAmountAtomic={(option, { decimals }) => (option.asset === "usdc" ? 250n : 125n) * 10n ** BigInt(decimals - 1)}
      walletClient={walletClient}
      pollInterval={500}
      onDepositSuccess={(payment) => onEvent(`success:${payment.tx_hash}`)}
      appearance={appearance}
    />
  );
}

function App() {
  const [events, setEvents] = useState<string[]>([]);
  return (
    <main>
      {params.has("deposit_mode") ? (
        <DepositMode onEvent={(event) => setEvents((e) => [...e, event])} />
      ) : params.has("element") ? (
        <Element />
      ) : params.has("picker") ? (
        <Picker />
      ) : params.has("modal") ? (
        <div className="modal" role="dialog" aria-label="Top up">
          <Picker />
        </div>
      ) : params.has("deposit") ? (
        <DepositAddress
          depositAddress={{ address, networks }} appearance={appearance}
          {...(params.has("client_secret") ? { clientSecret: params.get("client_secret") ?? "" } : {})}
          {...(params.has("api_base") ? { apiBase: params.get("api_base") ?? "" } : {})}
        />
      ) : (
        <Checkout
          clientSecret={params.get("client_secret") ?? ""}
          expectedAddress={params.get("expected_address") ?? ""}
          apiBase={params.get("api_base") ?? ""}
          appearance={appearance}
          pollInterval={500}
          walletClient={walletClient}
          onSuccess={() => setEvents((e) => [...e, "success"])}
          onExpire={() => setEvents((e) => [...e, "expire"])}
        />
      )}
      <p data-testid="events">{events.join(",")}</p>
    </main>
  );
}

const root = document.getElementById("root");
if (root !== null) {
  createRoot(root).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}
