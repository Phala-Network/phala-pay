import { useMutation } from "@tanstack/react-query";
import { ExternalLink, Wallet } from "lucide-react";
import type { ReactNode } from "react";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import type { Asset, Network } from "./api.js";
import { ChainIcon, TokenIcon } from "./chains.js";
import { ExplorerLink, InfoTip, errorMessage, wallet } from "./common.js";
import { tokenName, tokens } from "./format.js";
import type { PaidWith } from "./testTokens.js";

/**
 * A row of the test tokens card, the same for its action and its links: the token's mark and what
 * the row does, and at its end the kind of action (the wallet, or a link out).
 */
const TOKEN_ROW = "w-full justify-between";

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

/** A faucet, off the page: its mark and name, and the external-link icon at the row's end. */
function FaucetLink({ href, icon, title, children }: { href: string; icon: ReactNode; title?: string; children: ReactNode }) {
  return (
    <Button asChild variant="secondary" className={TOKEN_ROW}>
      <a href={href} target="_blank" rel="noreferrer" title={title}>
        <span className="flex items-center gap-2">
          {icon}
          {children}
        </span>
        <ExternalLink aria-hidden="true" />
      </a>
    </Button>
  );
}

/** The mint button: the token's mark and the amount, marked with the wallet that mints it. */
function MintButton({ token, label, mint }: { token: Asset } & ReturnType<typeof useMint>) {
  return (
    <Button type="button" variant="secondary" className={TOKEN_ROW} onClick={() => mint.mutate(token)} disabled={mint.isPending}>
      <span className="flex items-center gap-2">
        <TokenIcon asset={token.asset} className="size-4" />
        {mint.isPending && mint.variables.asset === token.asset ? "Confirm in your wallet…" : label(token)}
      </span>
      <Wallet aria-hidden="true" />
    </Button>
  );
}

/**
 * Where to get test tokens on the selected network, whatever token is selected: each mintable test
 * token's public mint, from the visitor's wallet (a button, marked with the wallet), of enough for
 * the payment at hand (`need`); then, as links out, another test token's issuer faucet and the
 * network's gas faucets.
 */
export function TestTokens({ network, need, className }: { network: Network; need: Need | null; className?: string }) {
  const mintable = network.assets.filter((each) => each.mintable);
  const fromFaucet = network.assets.find((each) => !each.mintable && each.faucet !== null);
  const { mint, label } = useMint(network, need);
  const chain = chainName(network);
  return (
    <div
      role="note"
      aria-label="Test tokens"
      className={cn("rounded-xl border border-dashed text-sm", className)}
    >
      <div className="flex items-center justify-between gap-2 px-5 pt-4 sm:px-6">
        <span className="font-medium">Need test tokens?</span>
        <InfoTip label="About test tokens">
          {mintable.map((each) =>
            each.minter === null
              ? `Test ${each.symbol} is free: its contract lets anyone mint it, so your own wallet mints it. `
              : `Test ${each.symbol} is free: a public faucet contract mints it to anyone, within the faucet's limits, so your own wallet mints it. `,
          )}
          {fromFaucet !== undefined && `Test ${fromFaucet.symbol} is free from Circle's faucet: pick ${chain} as the network there. `}
          Gas is {chain} ETH, also free, from a public faucet.
        </InfoTip>
      </div>
      <div className="flex flex-col gap-2 px-5 pt-3 pb-4 sm:px-6">
        {mintable.map((each) => (
          <MintButton key={each.asset} token={each} mint={mint} label={label} />
        ))}
        {fromFaucet !== undefined && fromFaucet.faucet !== null && (
          <FaucetLink href={fromFaucet.faucet} icon={<TokenIcon asset={fromFaucet.asset} className="size-4" />} title={`On the faucet, pick ${chain} as the network.`}>
            Circle {fromFaucet.symbol} faucet
          </FaucetLink>
        )}
        {network.faucet !== null && (
          <FaucetLink href={network.faucet} icon={<ChainIcon chainId={network.chain_id} className="size-4 rounded-full" />}>
            {chain} ETH faucets
          </FaucetLink>
        )}
      </div>
      <p aria-live="polite" className="border-t px-5 py-3 text-xs text-muted-foreground empty:hidden sm:px-6">
        {mint.isSuccess && (
          <>
            Minted: <ExplorerLink chainId={network.chain_id} kind="tx" value={mint.data} copy />
          </>
        )}
        {mint.isError && <span className="text-destructive">{errorMessage(mint.error, "Minting failed.")}</span>}
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
            <p aria-live="polite" className="text-xs text-muted-foreground empty:hidden">
              {mint.isSuccess && (
                <>
                  Minted: <ExplorerLink chainId={network.chain_id} kind="tx" value={mint.data} />. Pay again.
                </>
              )}
              {mint.isError && <span className="text-destructive">{errorMessage(mint.error, "Minting failed.")}</span>}
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
