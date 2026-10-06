import { useMutation } from "@tanstack/react-query";
import { Download } from "lucide-react";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { CodeBlock } from "@/components/ui/code-block";
import type { FlushCall, SweepGroup } from "./api.js";
import { TokenIcon } from "./chains.js";
import { Disclosure, Empty, ExplorerLink, TOUCH, downloadJson, errorMessage, wallet } from "./common.js";
import { day, duration, time, tokens } from "./format.js";
import { useSweeps } from "./queries.js";

/**
 * Sweeping is the merchant's own transaction (design D4): Phala Pay never sweeps and holds no
 * key. For each network and token, the product's SDK builds `factory.flush(treasury, salts,
 * token)` offline from the forwarders the service lists as sweepable, keeping only those its pins
 * derive, and a Safe Transaction Builder batch of the same call for a Safe treasury. The flush is
 * permissionless: whoever sends it pays the gas, and the funds can only reach the treasury.
 */
export function Sweeps({ ready }: { ready: boolean }) {
  const query = useSweeps(ready);
  const view = query.data ?? null;
  // The treasury, named once when every network shares it (as the staging Safe does); else each
  // network names its own.
  const groups = view?.groups ?? [];
  const shared = new Set(groups.map((group) => group.treasury.toLowerCase())).size === 1 ? groups[0] : undefined;
  return (
    <div className="flex flex-col gap-5">
      <p className="text-sm text-pretty text-muted-foreground">
        Payments stay in their forwarder addresses until the merchant sweeps them with{" "}
        <code className="font-mono text-[13px] text-foreground">factory.flush(treasury, salts, token)</code>, from its own
        wallet or its Safe, and pays the gas; Phala Pay never sweeps. Each forwarder pays only the treasury fixed in its
        address, so anyone may send it.
      </p>
      {shared !== undefined && (
        <p className="text-sm" data-testid="treasury">
          <span className="block text-muted-foreground">Treasury (Phala's staging Safe)</span>
          <ExplorerLink chainId={shared.chain_id} kind="address" value={shared.treasury} copy />
        </p>
      )}
      {view === null ? (
        <p className="text-sm text-muted-foreground" aria-busy="true">
          {query.isError ? "Sweeps are unavailable right now; retrying…" : "Loading…"}
        </p>
      ) : view.groups.length === 0 ? (
        <Empty>No network accepts payments right now.</Empty>
      ) : (
        <ul className="flex flex-col divide-y border-y">
          {view.groups.map((group) => (
            <li key={`${group.chain_id}-${group.asset}`}>
              <SweepRow group={group} showTreasury={shared === undefined} />
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** Seconds since `at`, ticking each second while shown. */
function useSecondsSince(at: number | undefined): number | null {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (at === undefined) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [at]);
  return at === undefined ? null : Math.max(0, Math.round(now / 1000 - at));
}

/**
 * A group's state when it is not current, as one muted line under its name, led by a neutral dot:
 * the service's last good data, shown while it refreshes (`stale`), or no data at all
 * (`unavailable`).
 */
function GroupNotice({ children, testId }: { children: string; testId: string }) {
  return (
    <p role="status" data-testid={testId} className="flex items-center gap-2 text-sm text-muted-foreground">
      <span aria-hidden="true" className="size-1.5 shrink-0 rounded-full bg-muted-foreground" />
      {children}
    </p>
  );
}

function StaleNotice({ asOf }: { asOf: number | undefined }) {
  const age = useSecondsSince(asOf);
  return (
    <GroupNotice testId="sweep-stale">
      {age === null ? "Showing the last update; refreshing" : `Updated ${duration(age)} ago; refreshing`}
    </GroupNotice>
  );
}

/**
 * A network and token: what is unswept and sweepable, the sweep's two ways, and the finalized
 * sweeps. While the service serves its last good data (`stale`), the balances show with its age and
 * no sweep is offered: the flush would be built from data that may have moved.
 */
function SweepRow({ group, showTreasury }: { group: SweepGroup; showTreasury: boolean }) {
  const send = useMutation({
    mutationFn: async (call: FlushCall) => {
      const { sendCall } = await wallet();
      return sendCall(group.chain_id, call);
    },
  });
  const { symbol, decimals } = group;
  const flush = group.stale ? undefined : group.flush[0];
  // Nothing was ever paid here: the row says only that.
  const empty = group.unswept_atomic === "0" && group.sweeps.length === 0;
  return (
    <section
      aria-label={`${symbol} on ${group.network}`}
      data-testid="sweep-group"
      data-stale={group.stale ? "true" : undefined}
      className="grid grid-cols-[1.5rem_minmax(0,1fr)] gap-x-3 gap-y-3 py-5"
    >
      <TokenIcon asset={group.asset} className="mt-0.5 size-6" />
      <div className="flex min-w-0 flex-col gap-3">
        <div className="flex flex-wrap items-start justify-between gap-x-4 gap-y-3">
          <div className="flex min-w-0 flex-col gap-1">
            <h4 className="text-sm font-semibold">
              {symbol} <span className="font-normal text-muted-foreground">on {group.network}</span>
            </h4>
            {group.unavailable ? (
              <GroupNotice testId="sweep-unavailable">Temporarily unavailable; retrying</GroupNotice>
            ) : empty ? (
              <p className="text-sm text-muted-foreground">Nothing to sweep.</p>
            ) : (
              <p data-testid="unswept" className="text-sm text-muted-foreground tabular-nums">
                {tokens(group.unswept_atomic, symbol, decimals)} unswept · sweepable now:{" "}
                <span className="text-foreground">
                  {tokens(group.final_unswept_atomic, symbol, decimals)} in {group.sweepable_forwarders} forwarder
                  {group.sweepable_forwarders === 1 ? "" : "s"}
                </span>
                {group.refused_forwarders > 0 && ` (${group.refused_forwarders} refused: not derivable from the pins)`}
              </p>
            )}
            {!group.unavailable && group.stale && <StaleNotice asOf={group.as_of} />}
          </div>
          {!group.unavailable && flush !== undefined && (
            <div className="flex flex-wrap gap-2">
              <Button type="button" size="sm" className={TOUCH} disabled={send.isPending} onClick={() => send.mutate(flush)}>
                {send.isPending ? "Confirm in your wallet…" : "Sweep from wallet"}
              </Button>
              <Button
                type="button"
                size="sm"
                variant="secondary"
                className={TOUCH}
                onClick={() => downloadJson(`phala-pay-sweep-${group.chain_id}-${group.asset}.json`, group.safe_batch)}
              >
                <Download aria-hidden="true" />
                Safe batch
              </Button>
            </div>
          )}
        </div>
        {showTreasury && (
          <p className="text-sm">
            <span className="text-muted-foreground">Treasury </span>
            <ExplorerLink chainId={group.chain_id} kind="address" value={group.treasury} />
          </p>
        )}
        {!group.unavailable && !group.stale && !empty && flush === undefined && (
          <p className="text-sm text-muted-foreground">Nothing sweepable yet: no final unswept balance.</p>
        )}
        {flush !== undefined && (
          <Disclosure summary={`The flush the SDK built (${group.flush.length} call${group.flush.length === 1 ? "" : "s"})`}>
            <CodeBlock value={JSON.stringify(group.flush, null, 2)} label="flush" />
          </Disclosure>
        )}
        <p className="text-sm text-muted-foreground empty:hidden" aria-live="polite" data-testid="flush-status">
          {send.isSuccess && (
            <>
              Flush sent: <ExplorerLink chainId={group.chain_id} kind="tx" value={send.data} />. It is indexed once final.
            </>
          )}
          {send.isError && errorMessage(send.error, "The wallet did not send it.")}
        </p>
        {/* Shown once there is one: the panel keeps a single empty state of its own. */}
        {!group.unavailable && group.sweeps.length > 0 && (
          <section aria-label={`Finalized sweeps of ${symbol} on ${group.network}`} className="flex flex-col gap-2">
            <h5 className="text-xs font-medium text-muted-foreground">Finalized sweeps</h5>
            <ul className="flex flex-col divide-y border-y text-sm">
              {group.sweeps.map((sweep) => (
                <li key={sweep.id} data-testid="sweep" className="flex flex-col gap-1 py-3">
                  <p className="flex flex-wrap justify-between gap-x-4 tabular-nums">
                    <span className="font-medium">{tokens(sweep.amount_atomic, symbol, decimals)}</span>
                    <span className="text-muted-foreground" title={time(sweep.created)}>{day(sweep.created)}</span>
                  </p>
                  <dl className="grid gap-x-3 gap-y-1 sm:grid-cols-[6rem_minmax(0,1fr)]">
                    <dt className="text-muted-foreground">Forwarder</dt>
                    <dd className="min-w-0"><ExplorerLink chainId={group.chain_id} kind="address" value={sweep.address} /></dd>
                    <dt className="text-muted-foreground">Transaction</dt>
                    <dd className="min-w-0"><ExplorerLink chainId={group.chain_id} kind="tx" value={sweep.tx_hash} /></dd>
                  </dl>
                </li>
              ))}
            </ul>
          </section>
        )}
      </div>
    </section>
  );
}
