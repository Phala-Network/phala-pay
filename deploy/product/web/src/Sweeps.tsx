import { useMutation } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import type { FlushCall, SweepGroup } from "./api.js";
import { ChainIcon, TokenIcon } from "./chains.js";
import { Detail, Details, Disclosure, Empty, ExplorerLink, InfoTip, downloadJson, errorMessage, wallet } from "./common.js";
import { time, tokens } from "./format.js";
import { useSweeps } from "./queries.js";
import { Requests } from "./Timeline.js";

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
  return (
    <div className="@container flex flex-col gap-5 text-xs">
      <p className="flex items-center gap-1.5 text-muted-foreground">
        <span>
          Phala Pay never sweeps: the merchant signs <code>factory.flush(treasury, salts, token)</code> and pays the
          gas.
        </span>
        <InfoTip label="About sweeps">
          Payments stay in their forwarder addresses until the merchant sweeps them, from its own wallet or from its
          Safe through the Transaction Builder. Each forwarder can pay only the treasury fixed in its address, so
          anyone may send the call. The service marks deposits swept from the finalized Flushed events.
        </InfoTip>
      </p>
      {view === null ? (
        <p className="text-muted-foreground" aria-busy="true">
          {query.isError ? "Sweeps are unavailable right now; retrying…" : "Loading…"}
        </p>
      ) : (
        <>
          {view.groups.length === 0 && <Empty>No network accepts payments right now.</Empty>}
          {view.groups.map((group) => (
            <SweepSection key={`${group.chain_id}-${group.asset}`} group={group} />
          ))}
          <Requests exchanges={view.api} title="API requests" id="sweeps-api-title" />
        </>
      )}
    </div>
  );
}

function SweepSection({ group }: { group: SweepGroup }) {
  const send = useMutation({
    mutationFn: async (call: FlushCall) => {
      const { sendCall } = await wallet();
      return sendCall(group.chain_id, call);
    },
  });
  const { symbol, decimals } = group;
  const flush = group.flush[0];
  return (
    <section
      aria-label={`${symbol} on ${group.network}`}
      data-testid="sweep-group"
      className="flex flex-col gap-4 rounded-lg border p-4"
    >
      <h3 className="flex items-center gap-2 text-sm font-medium">
        <TokenIcon asset={group.asset} className="size-5" />
        {symbol}
        <span className="flex items-center gap-1.5 font-normal text-muted-foreground">
          on <ChainIcon chainId={group.chain_id} className="size-4 rounded" />
          {group.network}
        </span>
      </h3>
      {group.unavailable ? (
        <p className="text-muted-foreground">Temporarily unavailable; retrying.</p>
      ) : (
        <div className="grid gap-6 @4xl:grid-cols-2 @4xl:gap-8">
        <div className="flex min-w-0 flex-col gap-4">
          <Details data-testid="unswept" className="tabular-nums">
            <Detail label="Unswept">{tokens(group.unswept_atomic, symbol, decimals)}</Detail>
            <Detail label="Final, sweepable">
              {tokens(group.final_unswept_atomic, symbol, decimals)} in {group.sweepable_forwarders} forwarder
              {group.sweepable_forwarders === 1 ? "" : "s"}
              {group.refused_forwarders > 0 && ` (${group.refused_forwarders} refused: not derivable from the pins)`}
            </Detail>
            <Detail label="Treasury">
              <ExplorerLink chainId={group.chain_id} kind="address" value={group.treasury} /> (this demo's is Phala's
              staging Safe)
            </Detail>
          </Details>
          {flush === undefined ? (
            <p className="text-muted-foreground">Nothing to sweep: no final unswept balance.</p>
          ) : (
            <div className="flex flex-col gap-3">
              <Disclosure summary={`The flush the SDK built (${group.flush.length} call${group.flush.length === 1 ? "" : "s"})`}>
                <pre className="max-h-60 overflow-auto rounded-lg bg-card p-3 dark:bg-muted font-mono text-xs leading-relaxed">
                  {JSON.stringify(group.flush, null, 2)}
                </pre>
              </Disclosure>
              <div className="flex flex-wrap gap-2">
                <Button type="button" disabled={send.isPending} onClick={() => send.mutate(flush)}>
                  {send.isPending ? "Confirm in your wallet…" : "Sign the flush from my wallet"}
                </Button>
                <Button
                  type="button"
                  variant="outline"
                  onClick={() => downloadJson(`phala-pay-sweep-${group.chain_id}-${group.asset}.json`, group.safe_batch)}
                >
                  Download Safe Transaction Builder batch
                </Button>
              </div>
              <p className="text-muted-foreground wrap-anywhere" aria-live="polite" data-testid="flush-status">
                {send.isSuccess && `Flush sent: ${send.data}. It is indexed once final.`}
                {send.isError && errorMessage(send.error, "The wallet did not send it.")}
              </p>
            </div>
          )}
        </div>
        {/* Shown once there is one: the panel keeps a single empty state of its own. */}
        {group.sweeps.length > 0 && (
          <div className="flex min-w-0 flex-col gap-3">
            <h4 className="text-sm font-medium">Finalized sweeps</h4>
            <Table className="text-xs">
                <TableHeader>
                  <TableRow>
                    <TableHead scope="col">Indexed</TableHead>
                    <TableHead scope="col">Forwarder</TableHead>
                    <TableHead scope="col">Amount</TableHead>
                    <TableHead scope="col">Flush transaction</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {group.sweeps.map((sweep) => (
                    <TableRow key={sweep.id} data-testid="sweep">
                      <TableCell>{time(sweep.created)}</TableCell>
                      <TableCell>
                        <ExplorerLink chainId={group.chain_id} kind="address" value={sweep.address} />
                      </TableCell>
                      <TableCell>{tokens(sweep.amount_atomic, symbol, decimals)}</TableCell>
                      <TableCell>
                        <ExplorerLink chainId={group.chain_id} kind="tx" value={sweep.tx_hash} />
                      </TableCell>
                    </TableRow>
                  ))}
                </TableBody>
            </Table>
          </div>
        )}
        </div>
      )}
    </section>
  );
}
