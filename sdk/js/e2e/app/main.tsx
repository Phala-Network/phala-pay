import { StrictMode, useState } from "react";
import { createRoot } from "react-dom/client";
import { createWalletClient, custom, isAddress } from "viem";
import { Checkout, DepositAddress } from "../../dist/react/index.js";
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
const className = params.has("frameless") ? "host-frameless" : "";

const address = "0x1111111111111111111111111111111111111111";
const token = "0x2222222222222222222222222222222222222222";
const networks = [11155111, 84532].map((chain_id) => ({
  chain_id,
  address,
  assets: ["pha", "usdc"].map((asset) => ({
    asset,
    contract: token,
    decimals: 18,
    payment_uri: `ethereum:${token}@${chain_id}/transfer?address=${address}`,
  })),
}));

function App() {
  const [events, setEvents] = useState<string[]>([]);
  return (
    <main>
      {params.has("deposit") ? (
        <DepositAddress depositAddress={{ address, networks }} appearance={appearance} className={className} />
      ) : (
        <Checkout
          clientSecret={params.get("client_secret") ?? ""}
          expectedAddress={params.get("expected_address") ?? ""}
          apiBase={params.get("api_base") ?? ""}
          appearance={appearance}
          className={className}
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
