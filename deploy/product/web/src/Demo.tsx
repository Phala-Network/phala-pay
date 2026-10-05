import type { Appearance } from "@phala/pay-react";
import "@phala/pay-react/styles.css";
import { QueryClientProvider, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { TooltipProvider } from "@/components/ui/tooltip";
import type { CreatedQuote, DepositAddressResponse, Selection } from "./api.js";
import { isTerminalApiError } from "./api.js";
import { Backend } from "./Backend.js";
import { queryErrorMessage } from "./common.js";
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
  const accountError = account.isError && account.data === undefined ? queryErrorMessage(account.error, "Account is") : null;
  const networksError = networks.isError && networks.data === undefined ? queryErrorMessage(networks.error, "Networks are") : null;
  const trustError = trust.isError && trust.data === undefined ? queryErrorMessage(trust.error, "Trust information is") : null;
  const timelineError = timeline.isError && timeline.data === undefined ? queryErrorMessage(timeline.error, "Timeline is") : null;
  const addressError = current.isError && current.data === undefined ? queryErrorMessage(current.error, "Deposit address is") : null;
  const accountPaused = account.isError && account.data !== undefined && isTerminalApiError(account.error);
  const networksPaused = networks.isError && networks.data !== undefined && isTerminalApiError(networks.error);
  const trustPaused = trust.isError && trust.data !== undefined && isTerminalApiError(trust.error);
  const timelinePaused = timeline.isError && timeline.data !== undefined && isTerminalApiError(timeline.error);
  const addressPaused = current.isError && current.data !== undefined && isTerminalApiError(current.error);

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
      <div className="grid items-start gap-x-8 gap-y-12 lg:grid-cols-[25rem_minmax(0,1fr)] xl:grid-cols-[27.5rem_minmax(0,1fr)] 2xl:gap-x-10">
        <Product
          account={account.data ?? null}
          accountError={accountError}
          accountPaused={accountPaused}
          networks={networks.data}
          networksError={networksError}
          networksPaused={networksPaused}
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
          accountError={accountError}
          accountPaused={accountPaused}
          selected={selected}
          timeline={timeline.data ?? null}
          timelineError={timelineError}
          timelinePaused={timelinePaused}
          trust={trust.data ?? null}
          trustError={trustError}
          trustPaused={trustPaused}
          address={current.data ?? null}
          addressError={addressError}
          addressPaused={addressPaused}
          networks={networks.data}
          networksError={networksError}
          networksPaused={networksPaused}
          onSelect={setSelected}
        />
      </div>
    </TooltipProvider>
  );
}
