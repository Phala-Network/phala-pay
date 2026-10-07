import { useMutation } from "@tanstack/react-query";
import { Download } from "lucide-react";
import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import type { FlushCall, SweepGroup } from "./api.js";
import { TokenIcon } from "./chains.js";
import { Empty, ExplorerLink, LearnMore, TOUCH, downloadJson, errorMessage, useShowAll, wallet } from "./common.js";
import { day, duration, time, tokens } from "./format.js";
import { useSweeps } from "./queries.js";

/** Names in prose: "A, B, and C". */
const list = new Intl.ListFormat("en", { type: "conjunction" });

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
  // Networks and tokens with nothing to sweep, current, and paying the one treasury named above: one
  // line for all of them; every other group a row of its own, the first two shown.
  const isIdle = (group: SweepGroup) => shared !== undefined && !group.unavailable && !group.stale && group.unswept_atomic === "0";
  const idle = groups.filter(isIdle);
  const active = useShowAll(groups.filter((group) => !isIdle(group)), 2);
  // Every network's finalized sweeps in one list, newest first, behind its button until asked for.
  const finalized = useShowAll(
    groups.filter((group) => !group.unavailable)
      .flatMap((group) => group.sweeps.map((sweep) => ({ group, sweep })))
      .sort((left, right) => right.sweep.created - left.sweep.created),
    0,
    true,
    "finalized sweeps",
  );
  return (
    <div className="flex flex-col gap-4">
      <p className="text-sm text-pretty text-muted-foreground">
        Payments wait in their forwarders until you sweep them.{" "}
        <LearnMore anchor="17-balance-sweeps-and-the-forwarder-export" topic="sweeps" />
      </p>
      {shared !== undefined && (
        <p className="flex flex-wrap items-center gap-x-2 text-sm" data-testid="treasury">
          <span className="text-muted-foreground">Treasury</span>
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
        <div className="flex flex-col gap-2">
          <ul className="flex flex-col divide-y border-y">
            {active.shown.map((group) => (
              <li key={`${group.chain_id}-${group.asset}`}>
                <SweepRow group={group} showTreasury={shared === undefined} />
              </li>
            ))}
            {idle.length > 0 && (
              <li data-testid="sweep-idle" className="py-3 text-sm text-pretty text-muted-foreground">
                Nothing to sweep:{" "}
                {list.format(idle.map((group) => `${group.symbol} on ${group.network}`))}.
              </li>
            )}
          </ul>
          {active.toggle}
        </div>
      )}
      {finalized.toggle}
      {finalized.shown.length > 0 && (
        <section aria-label="Finalized sweeps">
          <ul className="flex flex-col divide-y border-y text-sm">
            {finalized.shown.map(({ group, sweep }) => (
              <li key={sweep.id} data-testid="sweep" className="flex flex-col gap-1 py-2.5">
                <p className="flex flex-wrap justify-between gap-x-4 tabular-nums">
                  <span>
                    Swept <span className="font-medium">{tokens(sweep.amount_atomic, group.symbol, group.decimals)}</span>
                    <span className="text-muted-foreground"> on {group.network}</span>
                  </span>
                  <span className="text-muted-foreground" title={time(sweep.created)}>{day(sweep.created)}</span>
                </p>
                <p className="flex flex-wrap items-baseline gap-x-2">
                  <span className="text-muted-foreground">Transaction</span>
                  <ExplorerLink chainId={group.chain_id} kind="tx" value={sweep.tx_hash} />
                </p>
              </li>
            ))}
          </ul>
        </section>
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
 * A network and token: what is unswept and sweepable, and the sweep's two ways (the finalized
 * sweeps are listed together, under the rows). While the service serves its last good data
 * (`stale`), the balances show with its age and no sweep is offered: the flush would be built from
 * data that may have moved.
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
  // The flush sent from here, once the service indexes it as a finalized sweep, is listed below.
  const indexed = send.isSuccess && group.sweeps.some((sweep) => sweep.tx_hash.toLowerCase() === send.data.toLowerCase());
  // Nothing is left here to sweep (a row only when the treasury is not the shared one).
  const empty = group.unswept_atomic === "0";
  return (
    <section
      aria-label={`${symbol} on ${group.network}`}
      data-testid="sweep-group"
      data-stale={group.stale ? "true" : undefined}
      className="grid grid-cols-[1.5rem_minmax(0,1fr)] gap-x-3 gap-y-1 py-2.5"
    >
      <TokenIcon asset={group.asset} className="mt-0.5 size-6" />
      <div className="flex min-w-0 flex-col gap-2">
        <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
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
                <span className="text-foreground">
                  {tokens(group.final_unswept_atomic, symbol, decimals)} in {group.sweepable_forwarders} forwarder
                  {group.sweepable_forwarders === 1 ? "" : "s"}
                </span>{" "}
                sweepable
                {group.unswept_atomic !== group.final_unswept_atomic && ` of ${tokens(group.unswept_atomic, symbol, decimals)} unswept`}
                {group.refused_forwarders > 0 && ` (${group.refused_forwarders} refused: not derivable from the pins)`}
              </p>
            )}
            {!group.unavailable && group.stale && <StaleNotice asOf={group.as_of} />}
          </div>
          {!group.unavailable && flush !== undefined && (
            <div className="flex flex-wrap gap-2">
              <Button type="button" size="sm" className={TOUCH} disabled={send.isPending} onClick={() => send.mutate(flush)}>
                {send.isPending ? "Confirm in your wallet…" : <>Sweep<span className="sr-only"> from wallet</span></>}
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
        <p className="text-sm text-muted-foreground empty:hidden" aria-live="polite" data-testid="flush-status">
          {send.isSuccess && !indexed && (
            <>
              Flush sent: <ExplorerLink chainId={group.chain_id} kind="tx" value={send.data} />. It is indexed once final.
            </>
          )}
          {send.isError && errorMessage(send.error, "The wallet did not send it.")}
        </p>
      </div>
    </section>
  );
}
