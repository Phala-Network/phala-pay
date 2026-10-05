import type { CheckoutStatus } from "@phala/pay";
import type { Appearance } from "@phala/pay-react";
import { useQueryClient } from "@tanstack/react-query";
import { Check, CircleAlert, Cloud, CircleCheck, Copy, FlaskConical, Gift, Lock } from "lucide-react";
import { Suspense, lazy, useId, useState, type FormEvent, type ReactNode } from "react";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { InputGroup, InputGroupAddon, InputGroupInput, InputGroupText } from "@/components/ui/input-group";
import { Label } from "@/components/ui/label";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Separator } from "@/components/ui/separator";
import { Skeleton } from "@/components/ui/skeleton";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import type { Account, Asset, CreatedQuote, DepositAddressResponse, Network } from "./api.js";
import { TokenIcon, assetOf, networkOf, tokenFullName } from "./chains.js";
import { PRIMARY_BUTTON, ExplorerLink, InfoTip, describe, loadSdk } from "./common.js";
import { DepositAddressPanel } from "./DepositAddressPanel.js";
import { atomicAmount, dollars, percent, presetDollars, rate, signedDollars, tokenName } from "./format.js";
import { FundWallet, TestTokens, type Need } from "./Funding.js";
import { keys, useCreateQuote } from "./queries.js";
import type { PaidWith } from "./testTokens.js";
import { cn } from "@/lib/utils";

const Checkout = lazy(() => loadSdk().then((sdk) => ({ default: sdk.Checkout })));

export type Method = "quote" | "address";

/**
 * A choice card's selected state, the same for every single choice (amounts, tokens): the primary
 * border and ring, with the choice's radio filled.
 */
const CHOICE =
  "cursor-pointer has-data-checked:border-primary! has-data-checked:bg-transparent! has-data-checked:ring-1 has-data-checked:ring-primary";

const METHODS: { id: Method; label: string }[] = [
  { id: "quote", label: "Exact amount" },
  { id: "address", label: "Deposit address" },
];

/**
 * The product: a made-up cloud provider's billing page (Acme Cloud, at acme.example), as its
 * customer sees it in their browser, with Phala Pay inside.
 * Only customer-facing UI belongs here; what the backend sees is in ./Backend.
 */
export function Product({
  account,
  accountError,
  networks,
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
  account: Account | null;
  accountError: string | null;
  networks: Network[] | undefined;
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
  const picker = (
    <PaymentOptions
      networks={networks}
      network={network}
      asset={asset}
      onNetworkChange={(chainId) => setChoice({ chainId, asset: null })}
      onAssetChange={(value) => setChoice({ chainId: network?.chain_id ?? null, asset: value })}
    />
  );
  return (
    <div className="flex min-w-0 flex-col gap-3">
      <AreaLabel step={1} title="Your customer's view" text="Your app's billing page" />
      {/* An app in a browser window: the raised surface of the two. */}
      <section
        aria-labelledby="product-title"
        className="product-app overflow-hidden rounded-xl border bg-card text-card-foreground shadow-lg shadow-black/5 dark:shadow-black/40"
      >
        <div className="flex h-10 items-center gap-3 border-b bg-muted/40 px-4">
          <span className="flex gap-1.5" aria-hidden="true">
            <span className="size-2.5 rounded-full bg-foreground/15" />
            <span className="size-2.5 rounded-full bg-foreground/15" />
            <span className="size-2.5 rounded-full bg-foreground/15" />
          </span>
          {/* The address bar: the app's page, as a browser shows it. */}
          <span className="flex min-w-0 flex-1 justify-center" aria-hidden="true">
            <span className="flex h-6 w-full max-w-56 min-w-0 items-center justify-center gap-1.5 rounded-md border bg-background px-2.5 text-xs text-muted-foreground">
              <Lock className="size-3 shrink-0" />
              <span className="truncate">acme.example/billing</span>
            </span>
          </span>
          {(network?.testnet ?? true) && <TestnetBadge network={network?.name ?? "a"} />}
        </div>
        <div className="flex flex-col gap-6 p-5 sm:p-6">
          <div className="flex flex-col gap-5">
            {/* The app's own header: its name and page, and the signed-in workspace. */}
            <div className="flex items-center justify-between gap-4">
              <h2 id="product-title" className="flex min-w-0 items-center gap-2 text-sm font-semibold">
                <span
                  className="flex size-5 shrink-0 items-center justify-center rounded-md bg-foreground text-background"
                  aria-hidden="true"
                >
                  <Cloud className="size-3" />
                </span>
                <span className="truncate">
                  Acme Cloud<span className="font-normal text-muted-foreground"> · Billing</span>
                </span>
              </h2>
              <Workspace account={account} />
            </div>
            <div className="flex flex-col gap-1">
              <h3 id="balance-title" className="text-sm text-muted-foreground">
                Account balance
              </h3>
              <div
                className="text-3xl font-semibold tracking-tight tabular-nums"
                aria-live="polite"
                aria-labelledby="balance-title"
                data-testid="balance"
              >
                {account === null ? <Skeleton className="h-9 w-32" /> : dollars(account.balance)}
              </div>
            </div>
          </div>
          {accountError !== null && (
            <Alert variant="destructive">
              <CircleAlert aria-hidden="true" />
              <AlertDescription>Could not load the account: {accountError}</AlertDescription>
            </Alert>
          )}
          <Separator />
          <section aria-labelledby="pay-title" className="flex flex-col gap-4">
            <h3 id="pay-title" className="text-base font-semibold">
              Add credits
            </h3>
            <Tabs value={method} onValueChange={(value) => onMethodChange(value === "address" ? "address" : "quote")}>
              <TabsList aria-label="Payment method" className="w-full">
                {METHODS.map(({ id, label }) => (
                  <TabsTrigger key={id} value={id}>
                    {label}
                  </TabsTrigger>
                ))}
              </TabsList>
              <TabsContent value="quote" className="pt-4">
                {session === null || account === null ? (
                  <AmountPicker account={account} network={network} asset={asset} picker={picker} onQuote={onQuote} />
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
              <TabsContent value="address" className="pt-4">
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
        </div>
      </section>
      {network?.testnet === true && <TestTokens key={network.chain_id} network={network} need={need} className="mt-1" />}
    </div>
  );
}

/**
 * The caption above each of the demo's two areas, numbered as a guide's steps are; on a phone its
 * text on a line of its own.
 */
export function AreaLabel({ step, title, text }: { step: number; title: string; text: string }) {
  return (
    <p className="flex min-h-5 items-start gap-2.5 px-1 text-sm">
      <span
        className="flex size-5 shrink-0 items-center justify-center rounded-full border font-mono text-xs text-muted-foreground"
        aria-hidden="true"
      >
        {step}
      </span>
      <span className="min-w-0">
        <span className="font-medium">{title}</span>
        <span className="text-muted-foreground max-sm:block">
          <span className="max-sm:hidden"> · </span>
          {text}
        </span>
      </span>
    </p>
  );
}

/**
 * The product frame's testnet marker: in its title bar, where a customer looks for where they are;
 * it opens on a click or a tap to say what that means.
 */
function TestnetBadge({ network }: { network: string }) {
  return (
    <Popover>
      <PopoverTrigger asChild>
        <button type="button" data-testid="testnet-badge" className="rounded-full outline-none focus-visible:ring-2 focus-visible:ring-ring">
          <Badge variant="outline" className="gap-1 border-amber-500/40 bg-amber-500/10 text-amber-800 dark:text-amber-300">
            <FlaskConical aria-hidden="true" />
            Testnet
          </Badge>
        </button>
      </PopoverTrigger>
      <PopoverContent align="end" collisionPadding={12} className="w-64 p-3 text-xs leading-relaxed text-pretty text-muted-foreground">
        A demo on {network}: you pay with free test tokens, and no real money moves.
      </PopoverContent>
    </Popover>
  );
}

/** The demo workspace: its id, truncated, with a copy button. */
function Workspace({ account }: { account: Account | null }) {
  const [copied, setCopied] = useState(false);
  if (account === null) {
    return <Skeleton className="h-7 w-32 rounded-full" />;
  }
  const copy = () => {
    navigator.clipboard.writeText(account.account_id).then(
      () => {
        setCopied(true);
        setTimeout(() => setCopied(false), 1500);
      },
      () => undefined,
    );
  };
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          onClick={copy}
          aria-label={`Workspace ${account.account_id}, copy`}
          className="flex h-7 max-w-40 items-center gap-1.5 rounded-full border px-2.5 font-mono text-xs text-muted-foreground transition-colors outline-none hover:bg-muted focus-visible:ring-2 focus-visible:ring-ring"
        >
          <span className="size-1.5 shrink-0 rounded-full bg-success" aria-hidden="true" />
          <span className="truncate">{account.account_id}</span>
          {copied ? <Check className="size-3 shrink-0" aria-hidden="true" /> : <Copy className="size-3 shrink-0" aria-hidden="true" />}
        </button>
      </TooltipTrigger>
      <TooltipContent align="end">{copied ? "Copied" : "Your demo account, kept in this browser's cookie."}</TooltipContent>
    </Tooltip>
  );
}

function CheckoutSkeleton() {
  return (
    <div className="flex flex-col gap-3" aria-hidden="true">
      <Skeleton className="h-7 w-40" />
      <Skeleton className="h-4 w-56" />
      <Skeleton className="h-10 w-full" />
      <Skeleton className="h-10 w-full" />
    </div>
  );
}

/**
 * The network, then the token, as checkouts and wallets ask for them: a network select, and the
 * network's tokens as a list with each one's price terms. Shown even with one option each, so
 * the customer sees what they pay with (a test token, on a testnet) before paying.
 */
function PaymentOptions({
  networks,
  network,
  asset,
  onNetworkChange,
  onAssetChange,
}: {
  networks: Network[] | undefined;
  network: Network | undefined;
  asset: Asset | undefined;
  onNetworkChange: (chainId: number) => void;
  onAssetChange: (asset: string) => void;
}) {
  const id = useId();
  if (networks === undefined) {
    return (
      <div className="space-y-6" aria-hidden="true">
        <Skeleton className="h-10 w-full rounded-lg" />
        <Skeleton className="h-16 w-full rounded-lg" />
      </div>
    );
  }
  if (network === undefined || asset === undefined) {
    return <p className="text-sm text-muted-foreground">No network accepts payments right now.</p>;
  }
  return (
    <>
      <div className="space-y-2">
        <Label htmlFor={`${id}-network`}>Network</Label>
        <select
          id={`${id}-network`}
          className="h-10 w-full rounded-lg border border-input bg-background px-3 text-sm outline-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50"
          value={network.chain_id}
          onChange={(event) => onNetworkChange(Number(event.target.value))}
          data-testid="network-select"
        >
          {networks.map((each) => (
            <option key={each.chain_id} value={each.chain_id}>
              {each.name}
            </option>
          ))}
        </select>
      </div>
      <div className="space-y-2">
        <Label id={`${id}-token`} asChild>
          <span>Token</span>
        </Label>
        {/* Keyed by network: each network lists its own tokens. */}
        <RadioGroup
          key={network.chain_id}
          value={asset.asset}
          onValueChange={onAssetChange}
          aria-label="Token"
          className="auto-rows-fr gap-2"
        >
          {network.assets.map((each) => (
            <TokenOption key={each.asset} id={`${id}-token-${network.chain_id}-${each.asset}`} asset={each} testnet={network.testnet} />
          ))}
        </RadioGroup>
      </div>
    </>
  );
}

/**
 * A token row: its mark, symbol with the demo merchant's bonus, if any, and name; on the right its
 * price terms, which never shrink, and its radio. Where the row is narrow the bonus wraps under the
 * symbol, and every row takes the tallest's height.
 */
function TokenOption({ id, asset, testnet }: { id: string; asset: Asset; testnet: boolean }) {
  return (
    <FieldLabel htmlFor={id} data-testid="token-option" className={cn("w-full min-w-0", CHOICE)}>
      <Field orientation="horizontal" className="h-full min-w-0 items-center! gap-3 px-3! py-2.5!">
        <TokenIcon asset={asset.asset} />
        <span className="flex min-w-0 flex-1 flex-col items-start gap-0.5">
          <span className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-sm font-medium">
            {asset.symbol}
            {asset.bonus_bps > 0 && (
              <Badge className="bg-success/12 text-success" data-testid="token-bonus">
                <Gift aria-hidden="true" />+{percent(asset.bonus_bps)} bonus
              </Badge>
            )}
          </span>
          <span className="text-xs font-normal text-pretty text-muted-foreground">
            {testnet ? `Test ${tokenFullName(asset.asset)}` : tokenFullName(asset.asset)}
          </span>
        </span>
        {/* A stablecoin is valued at $1.00; any other token at the market rate, which a quote
            locks (the locked-rate line). */}
        <span className="shrink-0 text-sm font-normal tabular-nums" data-testid="token-price">
          {asset.pricing === "stablecoin" ? "$1.00" : "Market rate"}
        </span>
        <RadioGroupItem value={asset.asset} id={id} aria-label={tokenName(asset.symbol, testnet)} />
      </Field>
    </FieldLabel>
  );
}

function AmountPicker({
  account,
  network,
  asset,
  picker,
  onQuote,
}: {
  account: Account | null;
  network: Network | undefined;
  asset: Asset | undefined;
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
    <form onSubmit={submit} className="flex flex-col gap-6">
      <div className="space-y-2">
        <Label id={`${id}-amount-label`} asChild>
          <span>Amount</span>
        </Label>
        {/* Four across where each has room for its label (a full-width product), else two by two. */}
        <div className="@container">
          <RadioGroup
            value={String(preset)}
            onValueChange={(value) => setPreset(value === "custom" ? "custom" : Number(value))}
            aria-labelledby={`${id}-amount-label`}
            className="grid-cols-2 gap-2 @md:grid-cols-4"
          >
            {options.map((option) => (
              <FieldLabel key={option.value} htmlFor={`${id}-${option.value}`} className={CHOICE}>
                <Field orientation="horizontal" className="h-10 gap-2 px-3! py-0!">
                  <RadioGroupItem value={option.value} id={`${id}-${option.value}`} aria-label={option.label} />
                  <span className="text-sm font-medium tabular-nums">{option.label}</span>
                </Field>
              </FieldLabel>
            ))}
          </RadioGroup>
        </div>
      </div>
      {preset === "custom" && (
        <div className="space-y-2">
          <Label htmlFor={`${id}-amount`}>Custom amount (USD)</Label>
          <InputGroup className="h-10">
            <InputGroupAddon>
              <InputGroupText>$</InputGroupText>
            </InputGroupAddon>
            <InputGroupInput
              id={`${id}-amount`}
              inputMode="decimal"
              placeholder="25.00"
              className="tabular-nums"
              value={custom}
              onChange={(event) => setCustom(event.target.value)}
            />
          </InputGroup>
        </div>
      )}
      {picker}
      <div className="flex flex-col gap-2.5">
        <Button
          type="submit"
          size="lg"
          className={PRIMARY_BUTTON}
          disabled={quote.isPending || account === null || asset === undefined}
        >
          {quote.isPending ? "Creating quote…" : "Pay with crypto"}
        </Button>
        <p className="flex items-center justify-center gap-1.5 text-xs text-muted-foreground">
          Price locked for {minutes} minutes
          <InfoTip label="About paying for an exact amount">
            A quote locks the price for {minutes} minutes for an exact amount. Pay from a browser wallet, by QR code, or
            by sending the exact amount manually; another amount, or a late payment, is credited at the market rate
            instead.
          </InfoTip>
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
 * The quote's checkout: the SDK's `<Checkout>`, with the locked rate above it while it applies;
 * once credited, one confirmation of what the balance gained.
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
  const testnet = network?.testnet ?? true;
  const token = assetOf(network, session.asset);
  const bps = token?.bonus_bps ?? 0;
  const symbol = session.asset.toUpperCase();
  return (
    <div className="flex flex-col gap-4">
      {PAYABLE.has(status) && (
        <div data-testid="locked-rate" className="flex items-center justify-between gap-4 rounded-lg border bg-muted/40 px-4 py-3">
          <span className="flex min-w-0 items-center gap-2 text-sm text-muted-foreground">
            <TokenIcon asset={session.asset} className="size-5" />
            <span className="truncate">Locked rate · {tokenName(symbol, testnet)}</span>
          </span>
          <span className="shrink-0 text-sm font-semibold tabular-nums">{rate(symbol, session.exchange_rate)}</span>
        </div>
      )}
      {PAYABLE.has(status) && bps > 0 && (
        <p className="flex items-center gap-2 text-xs text-muted-foreground">
          <Gift className="size-3.5 text-success" aria-hidden="true" />
          Paying in {symbol} earns a +{percent(bps)} bonus, this demo merchant's promotion.
        </p>
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
      <Button type="button" variant="outline" className="w-full" onClick={onNewTopUp}>
        Add more credits
      </Button>
    </div>
  );
}

/**
 * The credited payment: the credit, the demo merchant's bonus on it, and the new total, with the
 * paying transaction. The amounts come from the product's own ledger as it applies the webhook.
 */
function Credited({ session, account, bps }: { session: CreatedQuote; account: Account; bps: number }) {
  const row = account.payments.find((each) => each.quote === session.quote && each.id.startsWith("dep_"));
  const bonus = row?.bonus ?? 0;
  const symbol = session.asset.toUpperCase();
  return (
    <Alert data-testid="payment-credited" role="status">
      <CircleCheck className="text-success!" aria-hidden="true" />
      <AlertTitle>Payment credited</AlertTitle>
      <AlertDescription className="text-foreground">
        <dl className="mt-1 grid w-full gap-1.5 tabular-nums">
          <div className="flex justify-between gap-3">
            <dt>Credit</dt>
            <dd>{dollars(session.amount)}</dd>
          </div>
          {bonus > 0 && (
            <div className="flex justify-between gap-3" data-testid="bonus-credited">
              <dt>
                {symbol} bonus{bps > 0 ? ` +${percent(bps)}` : ""}
              </dt>
              <dd className="text-success">{signedDollars(bonus)}</dd>
            </div>
          )}
          {bonus > 0 && (
            <div className="flex justify-between gap-3 border-t pt-1.5 font-medium">
              <dt>Total</dt>
              <dd>{dollars(session.amount + bonus)}</dd>
            </div>
          )}
          {row?.tx_hash != null && (
            <div className="flex items-center justify-between gap-3">
              <dt className="text-muted-foreground">Transaction</dt>
              <dd>
                <ExplorerLink chainId={session.chain_id} kind="tx" value={row.tx_hash} copy />
              </dd>
            </div>
          )}
        </dl>
        {bonus > 0 && <p className="mt-1 text-xs text-muted-foreground">The bonus is this demo merchant's promotion.</p>}
      </AlertDescription>
    </Alert>
  );
}
