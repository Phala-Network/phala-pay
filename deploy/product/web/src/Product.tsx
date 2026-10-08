import type { CheckoutStatus } from "@phala/pay";
import type { Appearance } from "@phala/pay-react";
import { useQueryClient } from "@tanstack/react-query";
import { CircleAlert, CircleCheck, Info } from "lucide-react";
import { Suspense, lazy, useId, useState, type FormEvent, type ReactNode } from "react";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from "@/components/ui/card";
import { ChoiceCard } from "@/components/ui/choice-card";
import { Field, FieldLabel } from "@/components/ui/field";
import { InputGroup, InputGroupAddon, InputGroupInput, InputGroupText } from "@/components/ui/input-group";
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { SegmentedControl, SegmentedControlItem } from "@/components/ui/segmented-control";
import { Skeleton } from "@/components/ui/skeleton";
import { StatusBadge } from "@/components/ui/status-badge";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";
import type { Account, Asset, CreatedQuote, DepositAddressResponse, Network } from "./api.js";
import { ChainIcon, TokenIcon, assetOf, networkOf, tokenFullName } from "./chains.js";
import { ExplorerLink, TOUCH, describe, loadSdk } from "./common.js";
import { QueryState, type QueryView } from "./queryView.js";
import { DepositAddressPanel } from "./DepositAddressPanel.js";
import { atomicAmount, dollars, percent, presetDollars, signedDollars, tokenName } from "./format.js";
import { FundWallet, TestTokens, type Need } from "./Funding.js";
import { keys, useCreateQuote } from "./queries.js";
import type { PaidWith } from "./testTokens.js";

// The payment form's fields put each label above its control, so every control in the card (the
// method tabs, the amounts, the network, the tokens, the pay button) starts on one left edge.

const Checkout = lazy(() => loadSdk().then((sdk) => ({ default: sdk.Checkout })));

export type Method = "quote" | "address";

const METHODS: { id: Method; label: string }[] = [
  { id: "quote", label: "Exact amount" },
  { id: "address", label: "Deposit address" },
];

/**
 * The customer's view: a made-up cloud provider's billing page (Acme Cloud), with Phala Pay inside.
 * Only customer-facing UI belongs here; what the backend sees is in ./Backend.
 */
export function Product({
  account: accountView,
  networks: networksView,
  method,
  onMethodChange,
  session,
  onQuote,
  onNewTopUp,
  onCredited,
  address,
  onAddress,
  appearance,
}: {
  account: QueryView<Account>;
  networks: QueryView<Network[]>;
  method: Method;
  onMethodChange: (method: Method) => void;
  session: CreatedQuote | null;
  onQuote: (created: CreatedQuote) => void;
  onNewTopUp: () => void;
  onCredited: () => void;
  address: DepositAddressResponse | null;
  onAddress: (created: DepositAddressResponse) => void;
  appearance: Appearance;
}) {
  const account = accountView.data ?? null;
  const networks = networksView.data;
  // The network, then the token, the customer pays with, for either method: the first offered
  // until they choose; a network's first token when they change the network.
  const [choice, setChoice] = useState<{ chainId: number | null; asset: string | null }>({
    chainId: null,
    asset: null,
  });
  const network = networks?.find((each) => each.chain_id === choice.chainId) ?? networks?.[0];
  const asset = network?.assets.find((each) => each.asset === choice.asset) ?? network?.assets[0];
  // The amount to send to the deposit address from the wallet, in the chosen token.
  const [sendAmount, setSendAmount] = useState("25");
  // The payment at hand, which the test token mint covers: the quote's, or the amount to send.
  let need: Need | null = null;
  if (method === "quote" && session !== null && session.chain_id === network?.chain_id) {
    need = { asset: session.asset, atomic: BigInt(session.amount_atomic) };
  } else if (method === "address" && address !== null && asset !== undefined) {
    const atomic = atomicAmount(sendAmount, asset.decimals);
    need = atomic === null ? null : { asset: asset.asset, atomic };
  }
  // Until the account loads, or while it cannot, nothing can be paid for.
  const unavailable = account === null && accountView.error !== null;
  const picker = (
    <PaymentOptions
      networks={networksView}
      network={network}
      asset={asset}
      onNetworkChange={(chainId) => setChoice({ chainId, asset: null })}
      onAssetChange={(value) => setChoice({ chainId: network?.chain_id ?? null, asset: value })}
    />
  );
  return (
    <Card role="region" aria-labelledby="product-title" className="flex-1">
      {/* One row: whose page this is (with the testnet mark while the network is a testnet), and
          the balance the payment moves. */}
      <CardHeader className="items-center py-3 sm:py-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <CardTitle id="product-title">Customer view</CardTitle>
            {(network?.testnet ?? true) && (
              <StatusBadge tone="warning" data-testid="testnet-notice">
                Testnet
              </StatusBadge>
            )}
          </div>
          <CardDescription className="mt-0.5">Acme Cloud · Billing</CardDescription>
        </div>
        <div className="text-right">
          <p id="balance-title" className="text-xs font-medium tracking-wider text-muted-foreground uppercase">
            Balance
          </p>
          <div
            className="text-2xl font-semibold tracking-tight tabular-nums"
            aria-live="polite"
            aria-labelledby="balance-title"
            data-testid="balance"
          >
            {account !== null ? dollars(account.balance) : unavailable ? "—" : <Skeleton className="ml-auto h-8 w-24" />}
          </div>
        </div>
      </CardHeader>
      <CardContent className="flex flex-col gap-4 py-4 lg:py-3">
        {accountView.data !== undefined && <QueryState view={accountView} />}
        {unavailable ? (
          <Alert variant="destructive">
            <CircleAlert aria-hidden="true" />
            <AlertTitle>Can't load your account</AlertTitle>
            <AlertDescription>{accountView.error}</AlertDescription>
          </Alert>
        ) : (
          <section aria-labelledby="pay-title" className="flex flex-col">
            <h4 id="pay-title" className="sr-only">
              Add credits
            </h4>
            <Tabs value={method} onValueChange={(value) => onMethodChange(value === "address" ? "address" : "quote")}>
              <TabsList aria-label="Payment method" className="w-full">
                {METHODS.map(({ id, label }) => (
                  <TabsTrigger key={id} value={id}>
                    {label}
                  </TabsTrigger>
                ))}
              </TabsList>
              <TabsContent value="quote" className="pt-3">
                {session === null || account === null ? (
                  <AmountPicker
                    account={account}
                    network={network}
                    asset={asset}
                    ready={account !== null && networks !== undefined}
                    picker={picker}
                    onQuote={onQuote}
                  />
                ) : (
                  <QuoteCheckout
                    key={session.quote}
                    session={session}
                    account={account}
                    network={networkOf(networks, session.chain_id)}
                    appearance={appearance}
                    onCredited={onCredited}
                    onNewTopUp={onNewTopUp}
                  />
                )}
              </TabsContent>
              <TabsContent value="address" forceMount hidden={method !== "address"} className="pt-3">
                {account === null ? (
                  <CheckoutSkeleton />
                ) : (
                  <DepositAddressPanel
                    account={account}
                    picker={picker}
                    network={network}
                    asset={asset}
                    appearance={appearance}
                    created={address}
                    onCreated={onAddress}
                    sendAmount={sendAmount}
                    onSendAmountChange={setSendAmount}
                  />
                )}
              </TabsContent>
            </Tabs>
          </section>
        )}
      </CardContent>
      {network?.testnet === true && (
        <CardFooter className="py-3 sm:py-3">
          <TestTokens key={network.chain_id} network={network} need={need} />
        </CardFooter>
      )}
    </Card>
  );
}

function CheckoutSkeleton() {
  return (
    <div className="flex flex-col gap-3" aria-hidden="true">
      <Skeleton className="h-7 w-40" />
      <Skeleton className="h-4 w-56" />
      <Skeleton className="h-11 w-full" />
      <Skeleton className="h-11 w-full" />
    </div>
  );
}

/**
 * The network, then the token, as checkouts and wallets ask for them: a network select, and the
 * network's tokens as a list with each one's price terms. Shown even with one option each, so
 * the customer sees what they pay with (a test token, on a testnet) before paying. While the
 * networks load, each field keeps its label over a placeholder of its control's size.
 */
function PaymentOptions({
  networks: networksView,
  network,
  asset,
  onNetworkChange,
  onAssetChange,
}: {
  networks: QueryView<Network[]>;
  network: Network | undefined;
  asset: Asset | undefined;
  onNetworkChange: (chainId: number) => void;
  onAssetChange: (asset: string) => void;
}) {
  const id = useId();
  const networks = networksView.data;
  if (networks === undefined && networksView.error !== null) {
    return <QueryState view={networksView} />;
  }
  if (networks !== undefined && (network === undefined || asset === undefined)) {
    return (
      <>
        <p className="text-sm text-muted-foreground">No network accepts payments right now.</p>
        <QueryState view={networksView} />
      </>
    );
  }
  return (
    <>
      <Field>
        <FieldLabel htmlFor={`${id}-network`}>Network</FieldLabel>
        {network === undefined ? (
          <Skeleton className="h-11 sm:h-10" />
        ) : (
          // The network's mark sits in the select's start, where a native option cannot show one.
          <div className="relative">
            <NativeSelect
              id={`${id}-network`}
              className={cn("pl-9", TOUCH)}
              value={network.chain_id}
              onChange={(event) => onNetworkChange(Number(event.target.value))}
              data-testid="network-select"
            >
              {networks?.map((each) => (
                <NativeSelectOption key={each.chain_id} value={each.chain_id}>
                  {each.name}
                </NativeSelectOption>
              ))}
            </NativeSelect>
            <ChainIcon
              chainId={network.chain_id}
              className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 rounded"
            />
          </div>
        )}
      </Field>
      <Field>
        <FieldLabel id={`${id}-token`} asChild>
          <span>Token</span>
        </FieldLabel>
        {network === undefined || asset === undefined ? (
          <div className="grid gap-2">
            <Skeleton className="h-13" />
            <Skeleton className="h-13" />
          </div>
        ) : (
          // Keyed by network: each network lists its own tokens.
          <RadioGroup
            key={network.chain_id}
            value={asset.asset}
            onValueChange={onAssetChange}
            aria-labelledby={`${id}-token`}
            className="auto-rows-fr"
          >
            {network.assets.map((each) => (
              <TokenOption key={each.asset} id={`${id}-token-${network.chain_id}-${each.asset}`} asset={each} testnet={network.testnet} />
            ))}
          </RadioGroup>
        )}
      </Field>
      <QueryState view={networksView} />
    </>
  );
}

/**
 * A token, in two lines that every token shares: its symbol and its price terms, then its name and
 * the demo merchant's bonus; its mark before them and its radio after.
 */
function TokenOption({ id, asset, testnet }: { id: string; asset: Asset; testnet: boolean }) {
  return (
    <ChoiceCard htmlFor={id} data-testid="token-option" className="min-h-13 py-2 leading-5">
      <TokenIcon asset={asset.asset} className="size-5" />
      <span className="grid min-w-0 flex-1 grid-cols-[minmax(0,1fr)_auto] gap-x-3">
        <span className="font-medium">{asset.symbol}</span>
        {/* A stablecoin is valued at $1.00; any other token at the market rate, which a quote
            locks (the locked-rate line). */}
        <span className="text-right tabular-nums" data-testid="token-price">
          {asset.pricing === "stablecoin" ? "$1.00" : "Market rate"}
        </span>
        <span className="text-muted-foreground">
          {testnet ? `Test ${tokenFullName(asset.asset)}` : tokenFullName(asset.asset)}
        </span>
        {asset.bonus_bps > 0 ? (
          <span className="text-right text-success" data-testid="token-bonus">
            +{percent(asset.bonus_bps)} bonus
          </span>
        ) : (
          <span />
        )}
      </span>
      <RadioGroupItem value={asset.asset} id={id} aria-label={tokenName(asset.symbol, testnet)} />
    </ChoiceCard>
  );
}

function AmountPicker({
  account,
  network,
  asset,
  ready,
  picker,
  onQuote,
}: {
  account: Account | null;
  network: Network | undefined;
  asset: Asset | undefined;
  /** Whether the account and its networks have loaded: until then the form is disabled. */
  ready: boolean;
  /** The network and token choice. */
  picker: ReactNode;
  onQuote: (created: CreatedQuote) => void;
}) {
  const [preset, setPreset] = useState<number | "custom">(2000);
  const [custom, setCustom] = useState("");
  const [invalid, setInvalid] = useState<string | null>(null);
  const quote = useCreateQuote();
  const id = useId();
  const min = Math.max(account?.min_amount ?? 100, asset?.min_amount ?? 0);
  const max = account?.max_amount ?? 100_000;

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const cents = preset === "custom" ? Math.round(Number(custom) * 100) : preset;
    if (!Number.isSafeInteger(cents) || cents < min || cents > max) {
      setInvalid(`Enter an amount between ${dollars(min)} and ${dollars(max)}.`);
      return;
    }
    if (network === undefined || asset === undefined) {
      return;
    }
    setInvalid(null);
    // The checkout's code loads while the quote is created.
    loadSdk().catch(() => undefined);
    quote.mutate({ amount: cents, chainId: network.chain_id, asset: asset.asset }, { onSuccess: onQuote });
  };
  const options = [
    ...(account?.presets ?? [500, 2000, 5000]).map((cents) => ({ value: String(cents), label: presetDollars(cents) })),
    { value: "custom", label: "Custom" },
  ];
  const error = invalid ?? (quote.error === null ? null : `Could not create the quote: ${describe(quote.error)}.`);
  const minutes = Math.round((asset?.quote_ttl_seconds ?? 900) / 60);

  return (
    <form onSubmit={submit} className="flex flex-col gap-3">
      <Field>
        <FieldLabel id={`${id}-amount-label`} asChild>
          <span>Amount</span>
        </FieldLabel>
        <SegmentedControl
          value={String(preset)}
          onValueChange={(value) => setPreset(value === "custom" ? "custom" : Number(value))}
          aria-labelledby={`${id}-amount-label`}
          disabled={!ready}
        >
          {options.map((option) => (
            <SegmentedControlItem key={option.value} value={option.value} className="tabular-nums">
              {option.label}
            </SegmentedControlItem>
          ))}
        </SegmentedControl>
      </Field>
      {preset === "custom" && (
        <Field>
          <FieldLabel htmlFor={`${id}-amount`}>
            Custom<span className="sr-only"> amount (USD)</span>
          </FieldLabel>
          <InputGroup className={TOUCH}>
            <InputGroupAddon>
              <InputGroupText>$</InputGroupText>
            </InputGroupAddon>
            <InputGroupInput
              id={`${id}-amount`}
              inputMode="decimal"
              placeholder="25.00"
              className="h-full tabular-nums"
              aria-invalid={invalid !== null}
              value={custom}
              onChange={(event) => setCustom(event.target.value)}
            />
          </InputGroup>
        </Field>
      )}
      {picker}
      <div className="flex flex-col gap-2 pt-1">
        <Button type="submit" size="lg" className="w-full" disabled={quote.isPending || !ready || asset === undefined}>
          {quote.isPending ? "Creating quote…" : "Pay with crypto"}
        </Button>
        <p className="text-sm text-pretty text-muted-foreground">
          Price locked for {minutes} minutes; other amounts at the market rate.
        </p>
      </div>
      {error !== null && (
        <Alert variant="destructive">
          <CircleAlert aria-hidden="true" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}
    </form>
  );
}

// While a quote can still be paid at its locked price; the SDK's status line says what happened
// after (expired, …) and holds the one countdown.
const PAYABLE: ReadonlySet<CheckoutStatus> = new Set(["loading", "waiting", "seen", "confirming"]);

/**
 * The quote's checkout: the SDK's `<Checkout>`, whose summary states the quote's amount; once
 * credited, one confirmation of what the balance gained.
 */
function QuoteCheckout({
  session,
  account,
  network,
  appearance,
  onCredited,
  onNewTopUp,
}: {
  session: CreatedQuote;
  account: Account;
  network: Network | undefined;
  appearance: Appearance;
  onCredited: () => void;
  onNewTopUp: () => void;
}) {
  const [status, setStatus] = useState<CheckoutStatus>("loading");
  const queryClient = useQueryClient();
  // The wallet that held too little for the quote, so the checkout sent nothing.
  const [short, setShort] = useState<PaidWith | null>(null);
  const token = assetOf(network, session.asset);
  const bps = token?.bonus_bps ?? 0;
  const symbol = session.asset.toUpperCase();
  return (
    <div className="flex flex-col gap-3">
      {/* While the quote is payable: the demo merchant's bonus, and from lg the way out beside it
          (on a phone the bonus has its line, and the way out is at the checkout's foot). */}
      {PAYABLE.has(status) && (
        <div className="flex items-center gap-3">
          {bps > 0 && (
            <p className="flex min-w-0 items-start gap-2 text-sm text-pretty text-muted-foreground">
              <Info className="mt-0.5 size-4 shrink-0" aria-hidden="true" />
              <span>
                <span className="text-success">+{percent(bps)} {symbol} bonus</span>, the demo merchant's promotion
              </span>
            </p>
          )}
          <Button type="button" variant="ghost" size="sm" className="ml-auto hidden shrink-0 lg:inline-flex" onClick={onNewTopUp}>
            Start a new top-up
          </Button>
        </div>
      )}
      {status === "credited" ? (
        <Credited session={session} account={account} bps={bps} />
      ) : (
        <Suspense fallback={<CheckoutSkeleton />}>
          <Checkout
            clientSecret={session.client_secret}
            expectedAddress={session.expected_address}
            apiBase={account.api_base}
            appearance={appearance}
            onChange={(state) => {
              setStatus(state.status);
              void queryClient.invalidateQueries({ queryKey: keys.timelines });
            }}
            onWalletError={(error, wallet) => setShort(error.code === "insufficient_balance" ? wallet : null)}
            onSuccess={onCredited}
          />
        </Suspense>
      )}
      {short !== null && status === "waiting" && network !== undefined && token !== undefined && (
        <FundWallet network={network} token={token} needed={BigInt(session.amount_atomic)} wallet={short} />
      )}
      {/* The next action: once credited, the card's own button; while paying, a quiet way out at
          the checkout's foot. */}
      {status === "credited" ? (
        <Button type="button" variant="secondary" size="lg" className="w-full" onClick={onNewTopUp}>
          Start a new top-up
        </Button>
      ) : (
        <div className={cn("justify-end border-t pt-2", PAYABLE.has(status) ? "flex lg:hidden" : "flex")}>
          <Button type="button" variant="ghost" size="sm" className={TOUCH} onClick={onNewTopUp}>
            Start a new top-up
          </Button>
        </div>
      )}
    </div>
  );
}

/**
 * The credited payment: the credit, the demo merchant's bonus on it, and the new total, side by
 * side, with the paying transaction. The amounts come from the product's own ledger as it applies
 * the webhook.
 */
function Credited({ session, account, bps }: { session: CreatedQuote; account: Account; bps: number }) {
  const row = account.payments.find((each) => each.quote === session.quote && each.id.startsWith("dep_"));
  const bonus = row?.bonus ?? 0;
  const symbol = session.asset.toUpperCase();
  return (
    <Alert variant="success" data-testid="payment-credited">
      <CircleCheck aria-hidden="true" />
      <AlertTitle>Payment credited</AlertTitle>
      <AlertDescription>
        {/* Labels on one row and values on the next, so the values stay level when a label wraps. */}
        <dl className="mt-2 grid grid-flow-col grid-cols-3 grid-rows-[auto_auto] gap-x-4 gap-y-1 tabular-nums [&>div]:row-span-2 [&>div]:grid [&>div]:grid-rows-subgrid [&_dt]:self-end">
          <div>
            <dt className="text-muted-foreground">Credit</dt>
            <dd className="font-medium">{dollars(session.amount)}</dd>
          </div>
          {bonus > 0 && (
            <div data-testid="bonus-credited">
              <dt className="text-muted-foreground">
                {symbol} bonus{bps > 0 ? ` +${percent(bps)}` : ""}
              </dt>
              <dd className="font-medium">{signedDollars(bonus)}</dd>
            </div>
          )}
          {bonus > 0 && (
            <div>
              <dt className="text-muted-foreground">Total</dt>
              <dd className="font-medium">{dollars(session.amount + bonus)}</dd>
            </div>
          )}
        </dl>
        {row?.tx_hash != null && (
          <p className="mt-3 flex flex-wrap items-center gap-x-3">
            <span className="text-muted-foreground">Transaction</span>
            <ExplorerLink chainId={session.chain_id} kind="tx" value={row.tx_hash} copy />
          </p>
        )}
      </AlertDescription>
    </Alert>
  );
}
