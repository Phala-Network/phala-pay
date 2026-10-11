import { useMutation } from "@tanstack/react-query";
import { CircleAlert, ExternalLink, Wallet } from "lucide-react";
import { useId, type ReactNode } from "react";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import type { Asset, Network } from "./api.js";
import { TokenIcon } from "./chains.js";
import { ExplorerLink, TOUCH, errorMessage, wallet } from "./common.js";
import { tokenName, tokens } from "./format.js";
import type { PaidWith } from "./testTokens.js";

// The mint's whole tokens: 1,000 test PHA (about $75 at staging's rate) unless a payment needs
// more, then that payment's amount, rounded up to a whole hundred tokens.
const DEFAULT_MINT = 1000n;
const MINT_STEP = 100n;

/** The atomic amount the mint button mints: enough for `needed` atomic units, at least the default. */
export function mintAmount(needed: bigint | undefined, decimals: number): bigint {
  const unit = 10n ** BigInt(decimals);
  const step = MINT_STEP * unit;
  const covering = needed === undefined ? 0n : ((needed + step - 1n) / step) * step;
  return covering > DEFAULT_MINT * unit ? covering : DEFAULT_MINT * unit;
}

/** A payment's amount of a token, in its atomic units. */
export interface Need {
  asset: string;
  atomic: bigint;
}

/** The network's name without "testnet", as faucets name it: `Base Sepolia`. */
function chainName(network: Network): string {
  return network.name.replace(/ testnet$/, "");
}

/**
 * The public mint of the network's test tokens, one at a time, from the visitor's wallet: `using`,
 * else the first browser wallet. A token mints enough for `need` when the payment is in it.
 */
function useMint(network: Network, need: Need | null, using?: PaidWith) {
  const amountOf = (token: Asset) =>
    mintAmount(need?.asset === token.asset ? need.atomic : undefined, token.decimals);
  const mint = useMutation({
    mutationFn: async (token: Asset) => {
      const { mintTestTokens } = await wallet();
      return mintTestTokens(network.chain_id, token, amountOf(token), using);
    },
  });
  const label = (token: Asset) => `Mint ${tokens(amountOf(token).toString(), `test ${token.symbol}`, token.decimals)}`;
  return { mint, label };
}

/** A faucet, off the page: its mark and name as a link, marked as leaving the page; 44px tall on a phone. */
function FaucetLink({ href, icon, title, children }: { href: string; icon?: ReactNode; title?: string; children: ReactNode }) {
  return (
    <a href={href} target="_blank" rel="noreferrer" title={title}
      className="inline-flex min-h-11 items-center gap-2 text-sm font-medium sm:min-h-8 text-foreground underline decoration-foreground/30 underline-offset-4 transition-colors hover:decoration-foreground">
      {icon}
      {children}
      <ExternalLink className="size-3.5 text-muted-foreground" aria-hidden="true" />
    </a>
  );
}

/** The mint button: the token's mark and the amount; 44px on a phone. */
function MintButton({ token, label, mint }: { token: Asset } & ReturnType<typeof useMint>) {
  return (
    <Button type="button" variant="secondary" size="sm" className={TOUCH} onClick={() => mint.mutate(token)} disabled={mint.isPending}>
      <TokenIcon asset={token.asset} className="size-4" />
      {mint.isPending && mint.variables.asset === token.asset ? "Confirm in your wallet…" : label(token)}
    </Button>
  );
}

/** A failed mint's reason, under the button that tried it. */
function MintError({ error }: { error: unknown }) {
  return (
    <span className="flex items-start gap-2">
      <CircleAlert className="mt-0.5 size-4 shrink-0 text-destructive" aria-hidden="true" />
      {errorMessage(error, "Minting failed.")}
    </span>
  );
}

/**
 * Where to get test tokens on the selected network, whatever token is selected: each mintable test
 * token's public mint, from the visitor's wallet, of enough for the payment at hand (`need`); then,
 * as links out, another test token's issuer faucet and the network's gas faucets.
 */
export function TestTokens({ network, need }: { network: Network; need: Need | null }) {
  const mintable = network.assets.filter((each) => each.mintable);
  const fromFaucet = network.assets.find((each) => !each.mintable && each.faucet !== null);
  const { mint, label } = useMint(network, need);
  const chain = chainName(network);
  const id = useId();
  // Two groups on one row, spaced apart: the mints from the wallet, then the faucets off the page
  // (the links name what each gives, a test token or gas); where the row is narrow, the faucets
  // wrap onto a line of their own. On a phone, the name heads the group, the mints are full-width
  // rows, and the faucets a list of 44px rows: one column, nothing left ragged.
  return (
    <div role="note" aria-labelledby={id} className="flex flex-col gap-3">
      {/* From lg the row speaks for itself: its name is for screen readers. */}
      <h4 id={id} className="text-sm font-medium text-muted-foreground lg:sr-only">
        Test tokens
      </h4>
      <div className="flex flex-col gap-x-6 gap-y-3 sm:flex-row sm:flex-wrap sm:items-center">
        <div className="grid gap-2 sm:flex sm:flex-wrap sm:items-center">
          {mintable.map((each) => (
            <MintButton key={each.asset} token={each} mint={mint} label={label} />
          ))}
        </div>
        <div className="flex flex-col items-start sm:flex-row sm:flex-wrap sm:items-center sm:gap-x-5 sm:gap-y-2">
          {fromFaucet !== undefined && fromFaucet.faucet !== null && (
            <FaucetLink href={fromFaucet.faucet} title={`On the faucet, pick ${chain} as the network.`}>
              Circle {fromFaucet.symbol} faucet
            </FaucetLink>
          )}
          {network.faucet !== null && <FaucetLink href={network.faucet}>{chain} ETH faucets</FaucetLink>}
        </div>
      </div>
      <p aria-live="polite" className="flex items-start gap-2 text-sm text-muted-foreground empty:hidden">
        {mint.isSuccess && (
          <>
            <span className="shrink-0">Minted:</span> <ExplorerLink chainId={network.chain_id} kind="tx" value={mint.data} copy />
          </>
        )}
        {mint.isError && <MintError error={mint.error} />}
      </p>
    </div>
  );
}

/**
 * Beside a payment the wallet could not cover (nothing was sent): how to get enough of the test
 * token, then pay again. The mintable token mints enough for the payment, to the wallet that tried
 * it (`wallet`, else the first browser wallet, as the page's own payments use); another test token
 * comes from its issuer's faucet.
 */
export function FundWallet({
  network,
  token,
  needed,
  wallet: using,
}: {
  network: Network;
  token: Asset;
  needed: bigint;
  wallet?: PaidWith | undefined;
}) {
  const { mint, label } = useMint(network, { asset: token.asset, atomic: needed }, using);
  const name = tokenName(token.symbol, network.testnet);
  if (!network.testnet || (!token.mintable && token.faucet === null)) {
    return null;
  }
  return (
    <Alert variant="warning" data-testid="fund-wallet">
      <Wallet aria-hidden="true" />
      <AlertTitle>Not enough {name} in your wallet</AlertTitle>
      <AlertDescription>
        {token.mintable
          ? "Mint enough to cover this payment, then pay again."
          : `Get ${name} from Circle's faucet, picking ${chainName(network)} as the network there, then pay again.`}
      </AlertDescription>
      <div className="col-start-2 mt-2 flex flex-col gap-2">
        {token.mintable ? (
          <>
            <MintButton token={token} mint={mint} label={label} />
            <p aria-live="polite" className="text-sm text-muted-foreground empty:hidden">
              {mint.isSuccess && (
                <>
                  Minted: <ExplorerLink chainId={network.chain_id} kind="tx" value={mint.data} />. Pay again.
                </>
              )}
              {mint.isError && <MintError error={mint.error} />}
            </p>
          </>
        ) : (
          token.faucet !== null && (
            <FaucetLink href={token.faucet} icon={<TokenIcon asset={token.asset} className="size-4" />}>
              Circle {token.symbol} faucet
            </FaucetLink>
          )
        )}
      </div>
    </Alert>
  );
}
