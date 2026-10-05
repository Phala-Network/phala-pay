import type { Appearance } from "@phala/pay-react";
import "@phala/pay-react/styles.css";
import { QueryClientProvider, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { TooltipProvider } from "@/components/ui/tooltip";
import type { CreatedQuote, DepositAddressResponse, Selection } from "./api.js";
import { Backend } from "./Backend.js";
import { describe } from "./common.js";
import { Product, type Method } from "./Product.js";
import { queryClient, keys, useAccount, useDepositAddress, useNetworks, useTimeline, useTrust } from "./queries.js";
import type { Theme } from "./theme.js";

export default function Demo({ theme }: { theme: Theme }) {
  return <QueryClientProvider client={queryClient}><DemoContent theme={theme} /></QueryClientProvider>;
}

function DemoContent({ theme }: { theme: Theme }) {
  const queryClient = useQueryClient();
  const account = useAccount();
  const networks = useNetworks();
  const trust = useTrust();
  const [method, setMethod] = useState<Method>("quote");
  const [session, setSession] = useState<CreatedQuote | null>(null);
  const [selected, setSelected] = useState<Selection | null>(null);
  const timeline = useTimeline(selected);
  // The deposit address as created, with the client secret the product's UI needs; the backend
  // follows it as the product reads it (its payments).
  const [address, setAddress] = useState<DepositAddressResponse | null>(null);
  const current = useDepositAddress(address !== null);

  // A new payment to the deposit address is followed as it arrives, as a quote is once created.
  const seenPayments = useRef<Set<string> | null>(null);
  useEffect(() => {
    if (current.data === undefined) {
      return;
    }
    const payments = current.data.deposit_address.payments.map((payment) => payment.deposit);
    const seen = seenPayments.current;
    const arrived = seen === null ? undefined : payments.find((deposit) => !seen.has(deposit));
    seenPayments.current = new Set(payments);
    if (arrived !== undefined) {
      setSelected({ kind: "deposit", id: arrived });
    }
  }, [current.data]);

  // The SDK's components take the page's theme tokens (src/index.css), their primary action the
  // page's one primary.
  const appearance: Appearance = {
    theme,
  };

  return (
    <TooltipProvider delayDuration={150}>
      <div className="grid items-start gap-x-8 gap-y-12 lg:grid-cols-[25rem_minmax(0,1fr)] xl:grid-cols-[27.5rem_minmax(0,1fr)] 2xl:gap-x-10">
        <Product
          account={account.data ?? null}
          accountError={account.error === null ? null : describe(account.error)}
          networks={networks.data}
          method={method}
          onMethodChange={setMethod}
          session={session}
          onQuote={(created) => {
            setSession(created);
            setSelected({ kind: "quote", id: created.quote });
          }}
          onNewTopUp={() => setSession(null)}
          onCredited={() => void queryClient.invalidateQueries({ queryKey: keys.account })}
          address={address}
          onAddress={setAddress}
          appearance={appearance}
        />
        <Backend
          account={account.data ?? null}
          selected={selected}
          timeline={timeline.data ?? null}
          trust={trust.data ?? null}
          address={current.data ?? null}
          networks={networks.data}
          onSelect={setSelected}
        />
      </div>
    </TooltipProvider>
  );
}
