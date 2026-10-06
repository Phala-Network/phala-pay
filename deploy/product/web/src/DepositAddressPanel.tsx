import type { Appearance } from "@phala/pay-react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { CircleAlert } from "lucide-react";
import { Suspense, lazy, useId, useState, type FormEvent, type ReactNode } from "react";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";
import type { Account, Asset, DepositAddressResponse, Network } from "./api.js";
import { ExplorerLink, TOUCH, describe, errorMessage, isShortOfTokens, loadSdk, wallet } from "./common.js";
import { assetOf, networkOf } from "./chains.js";
import { atomicAmount, dollars, price, signedDollars, statusLabel, tokenName, tokens } from "./format.js";
import { FundWallet } from "./Funding.js";
import { keys, useCreateDepositAddress, useNetworks } from "./queries.js";

const DepositAddress = lazy(() => loadSdk().then((sdk) => ({ default: sdk.DepositAddress })));

/**
 * The customer's single deposit address: one address for every supported token on every network,
 * credited at spot on arrival. The product creates it with its API key and the SDK recomputes it
 * from the pins before it is shown (./Backend shows that check); the browser follows its payments
 * with the address's `client_secret` through `<DepositAddress>`.
 */
export function DepositAddressPanel({
  account,
  picker,
  network,
  asset,
  appearance,
  created,
  onCreated,
  sendAmount,
  onSendAmountChange,
}: {
  account: Account;
  /** The network and token choice, shown until the address is; the address's view starts there. */
  picker: ReactNode;
  network: Network | undefined;
  asset: Asset | undefined;
  appearance: Appearance;
  created: DepositAddressResponse | null;
  onCreated: (created: DepositAddressResponse) => void;
  /** The amount to send from the browser wallet, as typed. */
  sendAmount: string;
  onSendAmountChange: (amount: string) => void;
}) {
  const show = useCreateDepositAddress();
  const queryClient = useQueryClient();

  if (created === null || created.client_secret === undefined) {
    return (
      <div className="flex flex-col gap-5">
        {picker}
        <div className="flex flex-col gap-2">
          <Button
            type="button"
            size="lg"
            className="w-full"
            onClick={() => {
              loadSdk().catch(() => undefined);
              show.mutate(undefined, { onSuccess: onCreated });
            }}
            disabled={show.isPending}
          >
            {show.isPending ? "Getting your address…" : "Show my deposit address"}
          </Button>
          <p className="text-sm text-pretty text-muted-foreground">
            Your workspace's own address, for any amount at any time: one address for every token on every network,
            credited at the market rate when a payment arrives.
          </p>
        </div>
        {show.error !== null && (
          <Alert variant="destructive">
            <CircleAlert aria-hidden="true" />
            <AlertDescription>Could not get the address: {describe(show.error)}.</AlertDescription>
          </Alert>
        )}
      </div>
    );
  }
  const view = created.deposit_address;
  return (
    <div className="flex flex-col gap-6">
      <Suspense
        fallback={
          <div className="flex flex-col gap-3" aria-hidden="true">
            <Skeleton className="mx-auto size-56" />
            <Skeleton className="h-11 w-full" />
          </div>
        }
      >
        <DepositAddress
          depositAddress={view}
          clientSecret={created.client_secret}
          apiBase={account.api_base}
          appearance={appearance}
          onChange={() => {
            void queryClient.invalidateQueries({ queryKey: keys.depositAddress });
            void queryClient.invalidateQueries({ queryKey: keys.account });
            void queryClient.invalidateQueries({ queryKey: keys.timelines });
          }}
          {...(network === undefined || asset === undefined ? {} : { chainId: network.chain_id, asset: asset.asset })}
        />
      </Suspense>
      <Credits account={account} />
      <PayFromWallet
        network={network}
        asset={asset}
        amount={sendAmount}
        onAmountChange={onSendAmountChange}
        to={
          view.networks.find((each) => each.chain_id === network?.chain_id)?.address ??
          view.address ??
          view.networks[0]?.address ??
          ""
        }
      />
    </div>
  );
}

/**
 * The address's recorded payments, each at the rate it was credited at (`deposit.exchange_rate`),
 * with the demo merchant's bonus when it earned one.
 */
function Credits({ account }: { account: Account }) {
  const networks = useNetworks().data;
  const deposits = account.payments.filter((row) => row.kind === "address" && row.id.startsWith("dep_"));
  if (deposits.length === 0) {
    return null;
  }
  return (
    <section aria-labelledby="credits-title" className="flex flex-col gap-2">
      <h5 id="credits-title" className="text-sm font-semibold">
        Your credits
      </h5>
      <ul className="flex flex-col divide-y border-y text-sm" data-testid="credits">
        {deposits.map((row) => {
          const network = networkOf(networks, row.chain_id);
          const token = assetOf(network, row.asset);
          const symbol = (row.asset ?? "token").toUpperCase();
          const testnet = network?.testnet ?? true;
          const reversed = row.status === "reversed" || row.status === "rejected";
          const valued = row.exchange_rate === null ? null : `${price(row.exchange_rate)} / ${symbol}`;
          return (
            <li key={row.id} data-testid="credit" className="flex items-start justify-between gap-3 py-2.5">
              <span className="flex min-w-0 flex-col">
                <span className="font-medium tabular-nums">
                  {tokens(row.amount_atomic, tokenName(symbol, testnet), token?.decimals)}
                </span>
                <span className="text-muted-foreground tabular-nums">
                  {row.status === "rejected"
                    ? statusLabel(row.status)
                    : valued === null
                      ? "Valuing…"
                      : row.status === "reversed"
                        ? `Credited at ${valued}, then reversed`
                        : `Credited at ${valued}`}
                </span>
              </span>
              <span className="flex shrink-0 flex-col items-end">
                <span className={cn("font-medium tabular-nums", reversed && "text-muted-foreground line-through")}>
                  {row.amount === null ? "—" : `+${dollars(row.amount)}`}
                </span>
                {row.bonus !== null && row.bonus > 0 && (
                  <span className="text-muted-foreground tabular-nums" data-testid="credit-bonus">
                    {signedDollars(row.bonus)} bonus
                  </span>
                )}
              </span>
            </li>
          );
        })}
      </ul>
    </section>
  );
}

function PayFromWallet({
  network,
  asset,
  amount,
  onAmountChange,
  to,
}: {
  network: Network | undefined;
  asset: Asset | undefined;
  amount: string;
  onAmountChange: (amount: string) => void;
  to: string;
}) {
  const [invalid, setInvalid] = useState<string | null>(null);
  const id = useId();
  const symbol = asset?.symbol ?? "tokens";
  const send = useMutation({
    mutationFn: async (atomic: bigint) => {
      if (network === undefined || asset === undefined) {
        throw new Error("Choose a network and a token first.");
      }
      const { transferTokens } = await wallet();
      return transferTokens(network.chain_id, asset, to, atomic);
    },
  });
  const submit = (event: FormEvent) => {
    event.preventDefault();
    const atomic = atomicAmount(amount, asset?.decimals ?? 18);
    if (atomic === null) {
      setInvalid(`Enter an amount of ${symbol}.`);
      return;
    }
    setInvalid(null);
    send.mutate(atomic);
  };

  return (
    <form
      className="flex flex-col gap-2 border-t pt-5"
      onSubmit={submit}
      aria-label="Pay to the deposit address from a browser wallet"
    >
      <FieldLabel htmlFor={id}>
        Send from your browser wallet ({tokenName(symbol, network?.testnet ?? true)})
      </FieldLabel>
      <div className="flex gap-2">
        <Input
          id={id}
          className={cn("tabular-nums", TOUCH)}
          inputMode="decimal"
          value={amount}
          aria-invalid={invalid !== null}
          // Fixed while the wallet confirms the transfer: the transfer is for this amount.
          disabled={send.isPending}
          onChange={(event) => {
            onAmountChange(event.target.value);
            // A refusal, and its mint, were for the amount before.
            send.reset();
          }}
        />
        <Button type="submit" variant="secondary" className={TOUCH} disabled={send.isPending || to === ""}>
          {send.isPending ? "Confirm in your wallet…" : "Send"}
        </Button>
      </div>
      <p className="text-sm text-muted-foreground wrap-anywhere empty:hidden" aria-live="polite">
        {invalid ?? (
          <>
            {send.isSuccess && (
              <>
                Sent: <ExplorerLink chainId={network?.chain_id} kind="tx" value={send.data} />
              </>
            )}
            {send.isError && errorMessage(send.error, "The wallet did not send it.")}
          </>
        )}
      </p>
      {invalid === null && isShortOfTokens(send.error) && network !== undefined && asset !== undefined && send.variables !== undefined && (
        <FundWallet network={network} token={asset} needed={send.variables} />
      )}
    </form>
  );
}
