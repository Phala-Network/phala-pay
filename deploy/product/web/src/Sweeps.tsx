import { useMutation } from "@tanstack/react-query";
import { Download } from "lucide-react";
import { Button } from "@/components/ui/button";
import { CodeBlock } from "@/components/ui/code-block";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import type { FlushCall, SweepGroup } from "./api.js";
import { TokenIcon } from "./chains.js";
import { Disclosure, Empty, ExplorerLink, TABLE, TOUCH, downloadJson, errorMessage, wallet } from "./common.js";
import { day, time, tokens } from "./format.js";
import { useSweeps } from "./queries.js";

/**
 * Sweeping is the merchant's own transaction (design D4): Phala Pay never sweeps and holds no
 * key. For each network and token, the product's SDK builds `factory.flush(treasury, salts,
 * token)` offline from the forwarders the service lists as sweepable, keeping only those its pins
 * derive, and a Safe Transaction Builder batch of the same call for a Safe treasury. The flush is
 * permissionless: whoever sends it pays the gas, and the funds can only reach the treasury.
 */
export function Sweeps() {
  const query = useSweeps();
  const view = query.data ?? null;
  // The treasury, named once when every network shares it (as the staging Safe does); else each
  // network names its own.
  const groups = view?.groups ?? [];
  const shared = new Set(groups.map((group) => group.treasury.toLowerCase())).size === 1 ? groups[0] : undefined;
  return (
    <div className="flex flex-col gap-5">
      <p className="text-sm text-pretty text-muted-foreground">
        Payments stay in their forwarder addresses until the merchant sweeps them with{" "}
        <code className="font-mono text-[13px]">factory.flush(treasury, salts, token)</code>, from its own wallet or
        its Safe, and pays the gas; Phala Pay never sweeps. Each forwarder pays only the treasury fixed in its
        address
        {shared !== undefined && (
          <>
            {" "}
            (this demo's: <ExplorerLink chainId={shared.chain_id} kind="address" value={shared.treasury} />, Phala's
            staging Safe)
          </>
        )}
        , so anyone may send it.
      </p>
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

/** A network and token: what is unswept and sweepable, the sweep's two ways, and the finalized sweeps. */
function SweepRow({ group, showTreasury }: { group: SweepGroup; showTreasury: boolean }) {
  const send = useMutation({
    mutationFn: async (call: FlushCall) => {
      const { sendCall } = await wallet();
      return sendCall(group.chain_id, call);
    },
  });
  const { symbol, decimals } = group;
  const flush = group.flush[0];
  // Nothing was ever paid here: the row says only that.
  const empty = group.unswept_atomic === "0" && group.sweeps.length === 0;
  return (
    <section
      aria-label={`${symbol} on ${group.network}`}
      data-testid="sweep-group"
      className="flex flex-col gap-3 py-4"
    >
      <div className="grid gap-x-4 gap-y-3 md:grid-cols-[minmax(0,1fr)_auto] md:items-center">
        <div className="flex min-w-0 items-start gap-3">
          <TokenIcon asset={group.asset} className="mt-0.5 size-6" />
          <div className="flex min-w-0 flex-col">
            <h4 className="text-sm font-semibold">
              {symbol} <span className="font-normal text-muted-foreground">on {group.network}</span>
            </h4>
            {group.unavailable ? (
              <p className="text-sm text-muted-foreground">Temporarily unavailable; retrying.</p>
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
            {showTreasury && (
              <p className="text-sm text-muted-foreground">
                Treasury <ExplorerLink chainId={group.chain_id} kind="address" value={group.treasury} />
              </p>
            )}
          </div>
        </div>
        {!group.unavailable && flush !== undefined && (
          <div className="flex flex-wrap gap-2 max-md:pl-9">
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
      {!group.unavailable && !empty && (
        <div className="flex flex-col gap-3 pl-9">
          {flush === undefined ? (
            <p className="text-sm text-muted-foreground">Nothing sweepable yet: no final unswept balance.</p>
          ) : (
            <Disclosure summary={`The flush the SDK built (${group.flush.length} call${group.flush.length === 1 ? "" : "s"})`}>
              <CodeBlock value={JSON.stringify(group.flush, null, 2)} label="flush" />
            </Disclosure>
          )}
          <p className="text-sm text-muted-foreground wrap-anywhere empty:hidden" aria-live="polite" data-testid="flush-status">
            {send.isSuccess && (
              <>
                Flush sent: <ExplorerLink chainId={group.chain_id} kind="tx" value={send.data} />. It is indexed once
                final.
              </>
            )}
            {send.isError && errorMessage(send.error, "The wallet did not send it.")}
          </p>
          {/* Shown once there is one: the panel keeps a single empty state of its own. */}
          {group.sweeps.length > 0 && (
            <Table className={TABLE} aria-label={`Finalized sweeps of ${symbol} on ${group.network}`}>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead scope="col">Finalized</TableHead>
                  <TableHead scope="col" className="hidden md:table-cell">
                    Forwarder
                  </TableHead>
                  <TableHead scope="col" className="text-right">
                    Amount
                  </TableHead>
                  <TableHead scope="col">Flush transaction</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {group.sweeps.map((sweep) => (
                  <TableRow key={sweep.id} data-testid="sweep">
                    <TableCell className="text-muted-foreground tabular-nums" title={time(sweep.created)}>
                      {day(sweep.created)}
                    </TableCell>
                    <TableCell className="hidden md:table-cell">
                      <ExplorerLink chainId={group.chain_id} kind="address" value={sweep.address} />
                    </TableCell>
                    <TableCell className="text-right tabular-nums">{tokens(sweep.amount_atomic, symbol, decimals)}</TableCell>
                    <TableCell>
                      <ExplorerLink chainId={group.chain_id} kind="tx" value={sweep.tx_hash} />
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </div>
      )}
    </section>
  );
}
