import type { Appearance } from "@phala/pay-react";
import "@phala/pay-react/styles.css";
import { QueryClientProvider, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { TooltipProvider } from "@/components/ui/tooltip";
import type { CreatedQuote, DepositAddressResponse, Selection } from "./api.js";
import { Backend } from "./Backend.js";
import { queryView } from "./queryView.js";
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
  const views = {
    account: queryView(account, "Account is"),
    networks: queryView(networks, "Networks are"),
    timeline: queryView(timeline, "Timeline is"),
    trust: queryView(trust, "Trust information is"),
    address: queryView(current, "Deposit address is"),
  };

  // Server-side settlement moves the balance and address view without a customer mutation.
  const observedTimeline = useRef<{ key: string; signature: string } | null>(null);
  useEffect(() => {
    if (selected === null || timeline.data === undefined) {
      observedTimeline.current = null;
      return;
    }
    const deposit = timeline.data.deposit;
    const signature = JSON.stringify([
      deposit?.status,
      deposit?.swept,
      deposit?.final,
      deposit?.amount_refunded_atomic,
      [...timeline.data.refunds].sort((left, right) => left.id.localeCompare(right.id))
        .map((refund) => [refund.id, refund.status]),
    ]);
    const key = `${selected.kind}:${selected.id}`;
    const previous = observedTimeline.current;
    observedTimeline.current = { key, signature };
    if (previous === null || previous.key !== key || previous.signature === signature) {
      return;
    }
    void queryClient.invalidateQueries({ queryKey: keys.account });
    void queryClient.invalidateQueries({ queryKey: keys.depositAddress });
  }, [selected, timeline.data, queryClient]);

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
      {/* Two sibling cards at their natural heights: the customer's view, then (beside it from lg)
          what the backend sees. */}
      <div className="grid items-start gap-6 lg:grid-cols-12">
        <div className="min-w-0 lg:col-span-5">
          <Product
            account={views.account}
            networks={views.networks}
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
        </div>
        <div className="min-w-0 lg:col-span-7">
          <Backend
            account={views.account}
            selected={selected}
            timeline={views.timeline}
            trust={views.trust}
            address={views.address}
            networks={views.networks}
            onSelect={setSelected}
          />
        </div>
      </div>
    </TooltipProvider>
  );
}
