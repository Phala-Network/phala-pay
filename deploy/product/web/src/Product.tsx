import { paymentOptions, samePaymentOption, type CheckoutStatus, type NetworkPaymentOption, type PaymentOption } from "@phala/pay";
import { PaymentOptionPicker, type Appearance, type PaymentElementStep } from "@phala/pay-react";
import { useQueryClient } from "@tanstack/react-query";
import { CircleAlert, CircleCheck, Info } from "lucide-react";
import { Suspense, lazy, useId, useState, type ReactNode } from "react";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from "@/components/ui/card";
import { Field, FieldLabel } from "@/components/ui/field";
import { InputGroup, InputGroupAddon, InputGroupInput, InputGroupText } from "@/components/ui/input-group";
import { SegmentedControl, SegmentedControlItem } from "@/components/ui/segmented-control";
import { Skeleton } from "@/components/ui/skeleton";
import { StatusBadge } from "@/components/ui/status-badge";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";
import type { Account, Asset, CreatedQuote, DepositAddressResponse, Network } from "./api.js";
import { assetOf, networkOf } from "./chains.js";
import { DEMO_PAYMENT_OPTIONS, DEMO_TESTNET } from "./content/site.js";
import { ExplorerLink, TOUCH, loadSdk } from "./common.js";
import { QueryState, type QueryView } from "./queryView.js";
import { DepositAddressPanel } from "./DepositAddressPanel.js";
import { atomicAmount, dollars, percent, presetDollars, signedDollars } from "./format.js";
import { FundWallet, TestTokens, type Need } from "./Funding.js";
import { keys, useCreateQuote } from "./queries.js";
import type { PaidWith } from "./testTokens.js";

// The payment form's fields put each label above its control, so every control in the card (the
// method tabs, the amounts, the network, the tokens, the pay button) starts on one left edge.

const PaymentElement = lazy(() => loadSdk().then((sdk) => ({ default: sdk.PaymentElement })));

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
  // The token on its network the customer pays with, for either method: the first offered until
  // they choose.
  const options = networks === undefined ? [] : paymentOptions(networks);
  const [choice, setChoice] = useState<PaymentOption | null>(null);
  const option = options.find((each) => samePaymentOption(each, choice)) ?? options[0];
  const network = option?.network;
  const asset = option?.token;
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
  const choose = (chosen: PaymentOption) => setChoice({ chain_id: chosen.chain_id, asset: chosen.asset });
  const picker = <PaymentOptions networks={networksView} options={options} option={option} onChange={choose} appearance={appearance} />;
  return (
    <Card role="region" aria-labelledby="product-title" className="flex-1 rounded-xl shadow-frame">
      {/* Whose page this is (with the testnet mark while the network is a testnet) over the
          merchant's name, and the balance the payment moves, its label beside it. */}
      <CardHeader className="items-center rounded-t-xl bg-muted/40">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <CardTitle id="product-title">Customer view</CardTitle>
            {(network?.testnet ?? DEMO_TESTNET) && (
              <StatusBadge tone="warning" data-testid="testnet-notice">
                Testnet
              </StatusBadge>
            )}
          </div>
          <CardDescription className="mt-1">Acme Cloud · Billing</CardDescription>
        </div>
        {/* The label over the figure, both from one left edge in a box as wide as most balances, so
            a figure that grows grows away from them and moves nothing. */}
        <div className="min-w-28">
          <p id="balance-title" className="mono-label text-muted-foreground">
            Balance
          </p>
          <div
            className="text-2xl font-semibold tracking-tight tabular-nums"
            aria-live="polite"
            aria-labelledby="balance-title"
            data-testid="balance"
          >
            {/* While the account loads, a new visitor's balance's size ($0.00). */}
            {account !== null ? dollars(account.balance) : unavailable ? "—" : <Skeleton className="h-8 w-16" />}
          </div>
        </div>
      </CardHeader>
      {/* The content takes the card's spare height, so the test tokens stay at its foot. */}
      <CardContent className="flex flex-1 flex-col gap-4">
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
                {/* Under 384px, labels padded 4px rather than 8px, so both fit the pill. */}
                {METHODS.map(({ id, label }) => (
                  <TabsTrigger key={id} value={id} className="max-[24rem]:group-data-[variant=default]/tabs-list:px-1">
                    {label}
                  </TabsTrigger>
                ))}
              </TabsList>
              {/* Kept mounted, as the address's tab is, so that a quote being paid survives a look at
                  the other method. */}
              <TabsContent value="quote" forceMount hidden={method !== "quote"} className="pt-6">
                <QuotePayment
                  account={account}
                  networks={networksView}
                  options={options}
                  option={option}
                  onOptionChange={choose}
                  session={session}
                  appearance={appearance}
                  onQuote={onQuote}
                  onCredited={onCredited}
                  onNewTopUp={onNewTopUp}
                />
              </TabsContent>
              <TabsContent value="address" forceMount hidden={method !== "address"} className="pt-6">
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
        <CardFooter>
          <TestTokens key={network.chain_id} network={network} need={need} />
        </CardFooter>
      )}
      {/* While the networks load (and as the page is prerendered), on the demo's testnets, the test
          tokens' row keeps its place: its name (on a phone) and its first button, so the card does
          not grow when they arrive. */}
      {network === undefined && networksView.error === null && DEMO_TESTNET && (
        <CardFooter aria-hidden="true">
          <div className="flex w-full flex-col gap-3">
            <p className="text-sm font-medium text-muted-foreground lg:hidden">Test tokens</p>
            <Skeleton className={cn("h-8 w-full sm:w-48", TOUCH)} />
          </div>
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

/** The site's network, with its tokens' terms, as the SDK's payment options carry it. */
type SiteOption = NetworkPaymentOption<Network>;

/**
 * Each token on each network with its price terms, shown even with one option, so the customer
 * sees what they pay with (a test token, on a testnet) before paying. While the networks load, a
 * placeholder of the options' size keeps their place.
 */
function PaymentOptions({
  networks,
  options,
  option,
  onChange,
  appearance,
}: {
  networks: QueryView<Network[]>;
  options: SiteOption[];
  option: SiteOption | undefined;
  onChange: (option: SiteOption) => void;
  appearance: Appearance;
}) {
  return (
    <OptionsState networks={networks} options={options} appearance={appearance}>
      <PaymentOptionPicker
        options={options}
        value={option}
        onChange={onChange}
        renderDetail={(each) => <TokenTerms asset={each.token} />}
        appearance={appearance}
      />
    </OptionsState>
  );
}

/** `children` once the networks offer an option; until then their placeholder, error, or absence. */
function OptionsState({
  networks,
  options,
  appearance,
  children,
}: {
  networks: QueryView<Network[]>;
  options: SiteOption[];
  appearance: Appearance;
  children: ReactNode;
}) {
  if (networks.data === undefined && networks.error !== null) {
    return <QueryState view={networks} />;
  }
  if (networks.data !== undefined && options.length === 0) {
    return (
      <>
        <p className="text-sm text-muted-foreground">No network accepts payments right now.</p>
        <QueryState view={networks} />
      </>
    );
  }
  return (
    <>
      {networks.data === undefined ? <OptionsSkeleton count={DEMO_PAYMENT_OPTIONS} appearance={appearance} /> : children}
      <QueryState view={networks} />
    </>
  );
}

/**
 * The options' places, `count` of them under their label, drawn with the SDK's own classes so that
 * each is as tall as the option that replaces it.
 */
function OptionsSkeleton({ count, appearance }: { count: number; appearance: Appearance }) {
  return (
    <div className="pp-root" data-theme={appearance.theme ?? "light"} aria-hidden="true">
      <fieldset className="pp-options" disabled>
        <legend className="pp-options__legend">Pay with</legend>
        {Array.from({ length: count }, (_, index) => (
          <div key={index} className="pp-option rounded-lg ring-1 ring-border">
            <Skeleton className="h-[2lh] flex-1" />
          </div>
        ))}
      </fieldset>
    </div>
  );
}

/**
 * A token's terms at the end of its option: its price (a stablecoin is valued at $1.00; any other
 * token at the market rate, which a quote locks), over the demo merchant's bonus.
 */
function TokenTerms({ asset }: { asset: Asset | undefined }) {
  if (asset === undefined) {
    return null;
  }
  return (
    <span className="flex flex-col items-end">
      <span className="tabular-nums" data-testid="token-price">
        {asset.pricing === "stablecoin" ? "$1.00" : "Market rate"}
      </span>
      {asset.bonus_bps > 0 && (
        <span className="text-success" data-testid="token-bonus">
          +{percent(asset.bonus_bps)} bonus
        </span>
      )}
    </span>
  );
}

// While a quote can still be paid at its locked price; the SDK's status line says what happened
// after (expired, …) and holds the one countdown.
const PAYABLE: ReadonlySet<CheckoutStatus> = new Set(["loading", "waiting", "seen", "confirming"]);

/**
 * An exact-amount payment: the amount, then the SDK's `<PaymentElement>`, which offers the tokens,
 * has the quote created when the customer pays (this page's own button submits it), and then shows
 * its checkout; once credited, one confirmation of what the balance gained.
 */
function QuotePayment({
  account,
  networks,
  options,
  option,
  onOptionChange,
  session,
  appearance,
  onQuote,
  onCredited,
  onNewTopUp,
}: {
  account: Account | null;
  networks: QueryView<Network[]>;
  options: SiteOption[];
  option: SiteOption | undefined;
  onOptionChange: (option: PaymentOption) => void;
  session: CreatedQuote | null;
  appearance: Appearance;
  onQuote: (created: CreatedQuote) => void;
  onCredited: () => void;
  onNewTopUp: () => void;
}) {
  const [preset, setPreset] = useState<number | "custom">(2000);
  const [custom, setCustom] = useState("");
  const [invalid, setInvalid] = useState<string | null>(null);
  const [step, setStep] = useState<PaymentElementStep>("options");
  // Each top-up's element: a new one starts from the options.
  const [round, setRound] = useState(0);
  const [status, setStatus] = useState<CheckoutStatus | null>(null);
  // The wallet that held too little for the quote, so the checkout sent nothing.
  const [short, setShort] = useState<PaidWith | null>(null);
  const quote = useCreateQuote();
  const queryClient = useQueryClient();
  const id = useId();
  const asset = option?.token;
  const min = Math.max(account?.min_amount ?? 100, asset?.min_amount ?? 0);
  const max = account?.max_amount ?? 100_000;
  const cents = preset === "custom" ? Math.round(Number(custom) * 100) : preset;
  const valid = Number.isSafeInteger(cents) && cents >= min && cents <= max;
  const ready = account !== null && networks.data !== undefined;
  const choosing = step === "options" || step === "creating";

  const createQuote = async (chosen: PaymentOption) => {
    const created = await quote.mutateAsync({ amount: cents, chainId: chosen.chain_id, asset: chosen.asset });
    onQuote(created);
    return { clientSecret: created.client_secret, expectedAddress: created.expected_address, apiBase: account?.api_base ?? "" };
  };
  const amounts = [
    ...(account?.presets ?? [500, 2000, 5000]).map((each) => ({ value: String(each), label: presetDollars(each) })),
    { value: "custom", label: "Custom" },
  ];
  const minutes = Math.round((asset?.quote_ttl_seconds ?? 900) / 60);
  const paying = session === null ? undefined : options.find((each) => samePaymentOption(each, session));
  const bps = paying?.token.bonus_bps ?? 0;

  return (
    // The form's fields apart; the checkout close to the bonus over it, as the receipt under it.
    <div className={cn("flex flex-col", choosing ? "gap-5" : "gap-3")}>
      {choosing && (
        <>
          <Field>
            <FieldLabel id={`${id}-amount-label`} asChild>
              <span>Amount</span>
            </FieldLabel>
            <SegmentedControl
              value={String(preset)}
              onValueChange={(value) => setPreset(value === "custom" ? "custom" : Number(value))}
              aria-labelledby={`${id}-amount-label`}
              disabled={!ready || step === "creating"}
            >
              {amounts.map((each) => (
                <SegmentedControlItem key={each.value} value={each.value} className="tabular-nums">
                  {each.label}
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
        </>
      )}
      {/* While the quote is payable: the demo merchant's bonus on its token. */}
      {step === "checkout" && session !== null && bps > 0 && PAYABLE.has(status ?? "loading") && (
        <p className="flex min-w-0 items-start gap-2 text-sm text-pretty text-muted-foreground">
          <Info className="mt-0.5 size-4 shrink-0" aria-hidden="true" />
          <span>
            <span className="text-success">+{percent(bps)} {session.asset.toUpperCase()} bonus</span>, the demo merchant's promotion
          </span>
        </p>
      )}
      {/* Once credited, the confirmation takes the checkout's place, as its next action does. */}
      <div hidden={status === "credited"}>
        <OptionsState networks={networks} options={options} appearance={appearance}>
          <Suspense fallback={<OptionsSkeleton count={options.length} appearance={appearance} />}>
            <PaymentElement
              key={round}
              id={`${id}-payment`}
              hideSubmit
              options={options}
              value={option}
              onOptionChange={onOptionChange}
              amount={valid ? cents : undefined}
              createQuote={createQuote}
              disabled={!ready}
              renderDetail={(each) => <TokenTerms asset={assetOf(networkOf(networks.data, each.chain_id), each.asset)} />}
              appearance={appearance}
              onChange={(state) => {
                setStep(state.step);
                setStatus(state.checkout?.status ?? null);
                if (state.step === "options") {
                  setShort(null);
                  onNewTopUp();
                  // Back from a quote, a new top-up starts from the default amount.
                  if (step === "checkout") {
                    setPreset(2000);
                    setCustom("");
                    setInvalid(null);
                  }
                }
                if (state.checkout !== null) {
                  void queryClient.invalidateQueries({ queryKey: keys.timelines });
                }
              }}
              onWalletError={(error, wallet) => setShort(error.code === "insufficient_balance" ? wallet : null)}
              onSuccess={onCredited}
            />
          </Suspense>
        </OptionsState>
      </div>
      {choosing && (
        <div className="flex flex-col gap-2">
          <Button
            type="submit"
            form={`${id}-payment`}
            size="lg"
            className="w-full"
            disabled={step === "creating" || !ready || option === undefined}
            onClick={(event) => {
              // An amount out of range stops the submit here, before a quote is asked for.
              setInvalid(valid ? null : `Enter an amount between ${dollars(min)} and ${dollars(max)}.`);
              if (!valid) event.preventDefault();
            }}
          >
            {step === "creating" ? "Creating quote…" : "Pay with crypto"}
          </Button>
          <p className="text-sm text-pretty text-muted-foreground">
            Price locked for {minutes} minutes; other amounts at the market rate.
          </p>
        </div>
      )}
      {choosing && invalid !== null && (
        <Alert variant="destructive">
          <CircleAlert aria-hidden="true" />
          <AlertDescription>{invalid}</AlertDescription>
        </Alert>
      )}
      {short !== null && status === "waiting" && session !== null && paying !== undefined && (
        <FundWallet network={paying.network} token={paying.token} needed={BigInt(session.amount_atomic)} wallet={short} />
      )}
      {status === "credited" && session !== null && account !== null && <Credited session={session} account={account} bps={bps} />}
      {status === "credited" && (
        <Button type="button" variant="secondary" size="lg" className="w-full" onClick={() => setRound((each) => each + 1)}>
          Start a new top-up
        </Button>
      )}
    </div>
  );
}

/**
 * The credited payment: the credit, the demo merchant's bonus on it, and the new total, side by
 * side, with the paying transaction. The amounts come from the product's own ledger as it applies
 * the webhook; until its row arrives, each keeps its place, so the confirmation does not grow.
 */
function Credited({ session, account, bps }: { session: CreatedQuote; account: Account; bps: number }) {
  const row = account.payments.find((each) => each.quote === session.quote && each.id.startsWith("dep_"));
  // The bonus once the ledger has credited it: `null` on a row polled before then, as no row.
  const bonus = row?.bonus ?? undefined;
  const symbol = session.asset.toUpperCase();
  const pending = <Skeleton className="h-[1lh] w-12" />;
  return (
    <Alert variant="success" data-testid="payment-credited">
      <CircleCheck aria-hidden="true" />
      <AlertTitle>Payment credited</AlertTitle>
      <AlertDescription>
        {/* On a phone, a receipt: each label beside its value, the values in a column at least as
            wide as a pending value's place. From sm, the labels on one row and the values on the
            next, so the values stay level when a label wraps; the bonus's column as wide as its
            label, the others sharing the rest. Either way a value arriving moves nothing. */}
        <dl className="mt-2 grid grid-cols-[minmax(0,1fr)_minmax(3rem,auto)] gap-x-2 gap-y-1 tabular-nums [&>div]:contents sm:grid-flow-col sm:grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)] sm:grid-rows-[auto_auto] sm:gap-x-4 sm:[&>div]:row-span-2 sm:[&>div]:grid sm:[&>div]:grid-rows-subgrid sm:[&_dt]:self-end">
          <div>
            <dt className="text-muted-foreground">Credit</dt>
            <dd className="text-right font-medium sm:text-left">{dollars(session.amount)}</dd>
          </div>
          {/* The bonus and the total where the token earns one: the token's terms say so at once. */}
          {bps > 0 && (
            <div data-testid={bonus === undefined ? undefined : "bonus-credited"}>
              <dt className="text-muted-foreground">
                {symbol} bonus +{percent(bps)}
              </dt>
              <dd className="text-right font-medium sm:text-left">{bonus === undefined ? pending : signedDollars(bonus)}</dd>
            </div>
          )}
          {bps > 0 && (
            <div>
              <dt className="text-muted-foreground">Total</dt>
              <dd className="text-right font-medium sm:text-left">{bonus === undefined ? pending : dollars(session.amount + bonus)}</dd>
            </div>
          )}
        </dl>
        {/* Until the transaction's hash arrives, its place: an unseen hash, as long and as wrapped. */}
        <p className="mt-3 flex flex-wrap items-center gap-x-3">
          <span className="text-muted-foreground">Transaction</span>
          {row?.tx_hash == null
            ? <span aria-hidden="true" className="invisible"><ExplorerLink chainId={session.chain_id} kind="tx" value={`0x${"0".repeat(64)}`} copy /></span>
            : <ExplorerLink chainId={session.chain_id} kind="tx" value={row.tx_hash} copy />}
        </p>
      </AlertDescription>
    </Alert>
  );
}
