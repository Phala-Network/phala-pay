import { useMutation } from "@tanstack/react-query";
import { ChevronDown, CircleAlert, CircleCheck } from "lucide-react";
import { useId, useState, type FormEvent } from "react";
import { isAddress, isHash, parseUnits } from "viem";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { CodeBlock } from "@/components/ui/code-block";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { Field, FieldLabel } from "@/components/ui/field";
import { Hash } from "@/components/ui/hash";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { StatusBadge } from "@/components/ui/status-badge";
import { cn } from "@/lib/utils";
import type { Deposit, Refund, Timeline } from "./api.js";
import { assetOf, networkOf } from "./chains.js";
import {
  DataItem,
  DataList,
  ExplorerLink,
  Subsection,
  TOUCH,
  describe,
  errorMessage,
  statusTone,
  useMediaQuery,
  wallet,
} from "./common.js";
import { statusLabel, tokens } from "./format.js";
import { useCancelRefund, useCreateRefund, useMarkRefundPaid, useNetworks } from "./queries.js";

/**
 * The refund flow (design D5): declare the refund, pay it from the treasury the deposit's address
 * pays, attach the transaction; the service verifies it at finality. On staging the product holds
 * no keys and the treasury is Phala's finance Safe, so the visitor plays the merchant's finance
 * team: a payment from the treasury succeeds, and one from any other wallet fails verification.
 */
export function Refunds({ timeline, deposit }: { timeline: Timeline; deposit: Deposit }) {
  const network = networkOf(useNetworks().data, deposit.chain_id);
  const token: RefundToken = {
    chainId: deposit.chain_id,
    symbol: (deposit.asset ?? "token").toUpperCase(),
    decimals: assetOf(network, deposit.asset)?.decimals ?? 18,
  };
  const treasury = network?.treasury ?? timeline.refunds[0]?.treasury ?? "";
  const refundable = deposit.final && (deposit.status === "credited" || deposit.status === "rejected");
  return (
    <Subsection title="Refunds" id="refunds-title">
      <p className="text-sm text-pretty text-muted-foreground">
        The merchant refunds from its own treasury: declare the refund, pay it from the treasury this deposit's
        address pays, then attach the transaction, which Phala Pay verifies once final. On this demo the treasury{" "}
        <ExplorerLink chainId={token.chainId} kind="address" value={treasury} /> is Phala's finance Safe, which you
        do not control: a refund you pay from your own wallet fails verification with{" "}
        <code className="font-mono text-[13px]">sender_mismatch</code>, as it should.
      </p>
      {refundable ? (
        <RefundForm deposit={deposit} token={token} />
      ) : (
        <p data-testid="refund-unavailable" className="text-sm text-muted-foreground">
          {deposit.status === "reversed"
            ? "A reversed deposit cannot be refunded."
            : "Refunds need a final deposit (the service answers 400 deposit_not_final before)."}
        </p>
      )}
      {timeline.refunds.length > 0 && (
        <ul className="flex flex-col divide-y border-y" aria-label="Refunds of this deposit">
          {timeline.refunds.map((refund) => (
            <RefundItem key={refund.id} refund={refund} token={token} />
          ))}
        </ul>
      )}
    </Subsection>
  );
}

/** The refunded deposit's token: its chain, symbol, and decimals. */
interface RefundToken {
  chainId: number;
  symbol: string;
  decimals: number;
}

/** One row: the amount, the destination, and the button. */
function RefundForm({ deposit, token }: { deposit: Deposit; token: RefundToken }) {
  const { symbol, decimals } = token;
  const remaining = BigInt(deposit.amount_atomic) - BigInt(deposit.amount_refunded_atomic);
  const [amount, setAmount] = useState("");
  const [destination, setDestination] = useState(deposit.from_address);
  const [invalid, setInvalid] = useState<string | null>(null);
  const create = useCreateRefund();
  const amountId = useId();
  const destinationId = useId();
  // On a phone the address wraps over two lines, every character in view; from sm it fits one.
  const wide = useMediaQuery("(min-width: 40rem)");
  const submit = (event: FormEvent) => {
    event.preventDefault();
    let atomic: bigint;
    try {
      atomic = parseUnits(amount.trim(), decimals);
    } catch {
      setInvalid(`Enter an amount of ${symbol}.`);
      return;
    }
    if (atomic <= 0n || atomic > remaining) {
      setInvalid(`Enter at most ${tokens(remaining.toString(), symbol, decimals)}.`);
      return;
    }
    const address = destination.trim();
    if (!isAddress(address)) {
      setInvalid("Enter a 0x address for the destination.");
      return;
    }
    setInvalid(null);
    create.mutate(
      { deposit: deposit.id, amountAtomic: atomic.toString(), destinationAddress: address },
      { onSuccess: () => setAmount("") },
    );
  };
  const error = invalid ?? (create.error === null ? null : `Could not declare the refund: ${describe(create.error)}.`);
  return (
    <form className="flex flex-col gap-3" onSubmit={submit} aria-label="Declare a refund">
      <div className="grid gap-3 sm:grid-cols-[7rem_minmax(0,1fr)_auto] sm:items-end">
        <Field>
          <FieldLabel htmlFor={amountId}>Amount ({symbol})</FieldLabel>
          <Input
            id={amountId}
            inputMode="decimal"
            className={cn("tabular-nums", TOUCH)}
            aria-describedby={`${amountId}-limit`}
            value={amount}
            onChange={(event) => setAmount(event.target.value)}
          />
        </Field>
        <Field>
          <FieldLabel htmlFor={destinationId}>Destination</FieldLabel>
          {wide ? (
            <Input
              id={destinationId}
              className="font-mono md:text-[13px]"
              spellCheck={false}
              value={destination}
              onChange={(event) => setDestination(event.target.value)}
            />
          ) : (
            <Textarea
              id={destinationId}
              rows={2}
              className="resize-none font-mono break-all"
              spellCheck={false}
              value={destination}
              onChange={(event) => setDestination(event.target.value)}
            />
          )}
        </Field>
        <Button type="submit" variant="secondary" className={TOUCH} disabled={create.isPending}>
          {create.isPending ? "Declaring…" : "Declare refund"}
        </Button>
      </div>
      <p id={`${amountId}-limit`} className="text-sm text-muted-foreground">
        At most {tokens(remaining.toString(), symbol, decimals)}, to the payer's address unless you change it.
      </p>
      {error !== null && <ErrorAlert text={error} />}
    </form>
  );
}

/**
 * A refund: one line (its id, amount, and status) that opens to the transfer to pay, the form that
 * attaches it, and the outcome. Open from the start while it waits for a payment.
 */
function RefundItem({ refund, token }: { refund: Refund; token: RefundToken }) {
  const { chainId, symbol, decimals } = token;
  const [open, setOpen] = useState(refund.status === "pending");
  const [hash, setHash] = useState("");
  const [logIndex, setLogIndex] = useState("");
  const [invalid, setInvalid] = useState<string | null>(null);
  const mark = useMarkRefundPaid();
  const cancel = useCancelRefund();
  // "Pay it from my wallet instead": the visitor's own wallet pays the transfer, whose hash is
  // then attached like the treasury's would be.
  const pay = useMutation({
    mutationFn: async (transfer: NonNullable<Refund["transfer"]>) => {
      const { transferTokens } = await wallet();
      return transferTokens(chainId, { contract: transfer.token, symbol, decimals }, transfer.to, BigInt(transfer.amount_atomic));
    },
    onSuccess: setHash,
  });
  const hashId = useId();
  const indexId = useId();
  const markPaid = (event: FormEvent) => {
    event.preventDefault();
    const trimmed = hash.trim();
    if (!isHash(trimmed)) {
      setInvalid("Enter the 0x transaction hash of the payment.");
      return;
    }
    const index = logIndex.trim() === "" ? null : Number(logIndex);
    if (index !== null && !(Number.isSafeInteger(index) && index >= 0)) {
      setInvalid("The receipt log index is a whole number.");
      return;
    }
    setInvalid(null);
    mark.mutate({ refund: refund.id, transactionHash: trimmed, receiptLogIndex: index });
  };
  const pending = mark.isPending || cancel.isPending || pay.isPending;
  const failed = mark.error ?? cancel.error;
  const error = invalid ?? (failed === null ? null : describe(failed));
  const transfer = refund.transfer;
  return (
    <li data-testid="refund" data-refund={refund.id} data-status={refund.status}>
      <Collapsible open={open} onOpenChange={setOpen}>
        <CollapsibleTrigger
          className="group/trigger grid min-h-11 w-full grid-cols-[minmax(0,1fr)_auto_auto] items-center gap-x-3 py-2 text-left text-sm transition-colors hover:bg-muted/60"
          aria-label={`Refund ${refund.id}, ${tokens(refund.amount_atomic, symbol, decimals)}, ${statusLabel(refund.status)}`}
        >
          <span className="flex min-w-0 flex-wrap items-baseline gap-x-3">
            <span className="font-medium tabular-nums">{tokens(refund.amount_atomic, symbol, decimals)}</span>
            <Hash value={refund.id} />
          </span>
          <StatusBadge tone={statusTone(refund.status)}>{statusLabel(refund.status)}</StatusBadge>
          <ChevronDown
            className="size-4 text-muted-foreground transition-transform group-data-[state=open]/trigger:rotate-180 motion-reduce:transition-none"
            aria-hidden="true"
          />
        </CollapsibleTrigger>
        <CollapsibleContent className="flex flex-col gap-4 pb-4">
          <DataList>
            <DataItem label="To">
              <ExplorerLink chainId={chainId} kind="address" value={refund.destination_address} />
            </DataItem>
          </DataList>
          {transfer !== null && (
            <>
              <div className="flex flex-col gap-2" data-testid="refund-transfer">
                <p className="text-sm font-medium">Pay exactly this transfer from the treasury, then attach its hash:</p>
                <DataList className="border-l pl-3">
                  <DataItem label="From (treasury)">
                    <Hash value={transfer.from} copyLabel="Copy treasury address" />
                  </DataItem>
                  <DataItem label="Token contract">
                    <Hash value={transfer.token} copyLabel="Copy token contract" />
                  </DataItem>
                  <DataItem label="To">
                    <Hash value={transfer.to} copyLabel="Copy destination address" />
                  </DataItem>
                  <DataItem label="Amount" className="tabular-nums">
                    {tokens(transfer.amount_atomic, symbol, decimals)}{" "}
                    <span className="font-mono text-[13px] text-muted-foreground">({transfer.amount_atomic})</span>
                  </DataItem>
                </DataList>
                <p className="text-sm text-muted-foreground">Calldata</p>
                <CodeBlock value={transfer.data} label="calldata" className="break-all whitespace-pre-wrap" />
              </div>
              <form className="flex flex-col gap-3" onSubmit={markPaid} aria-label={`Mark refund ${refund.id} paid`}>
                <Field>
                  <FieldLabel htmlFor={hashId}>Transaction hash of the payment</FieldLabel>
                  <Input
                    id={hashId}
                    className={cn("font-mono md:text-[13px]", TOUCH)}
                    spellCheck={false}
                    placeholder="0x…"
                    value={hash}
                    onChange={(event) => setHash(event.target.value)}
                  />
                </Field>
                <Field>
                  <FieldLabel htmlFor={indexId}>Receipt log index (optional)</FieldLabel>
                  <Input
                    id={indexId}
                    inputMode="numeric"
                    className={cn("tabular-nums", TOUCH)}
                    aria-describedby={`${indexId}-hint`}
                    value={logIndex}
                    onChange={(event) => setLogIndex(event.target.value)}
                  />
                  <p id={`${indexId}-hint`} className="text-sm text-muted-foreground">
                    When one transaction pays several refunds.
                  </p>
                </Field>
                <div className="flex flex-wrap gap-2">
                  <Button type="submit" variant="secondary" className={TOUCH} disabled={pending}>
                    {mark.isPending ? "Submitting…" : "Mark paid"}
                  </Button>
                  <Button type="button" variant="ghost" className={TOUCH} disabled={pending} onClick={() => cancel.mutate(refund.id)}>
                    {cancel.isPending ? "Canceling…" : "Cancel refund"}
                  </Button>
                </div>
              </form>
              <div className="flex flex-col items-start gap-1 border-t pt-4">
                <p className="text-sm text-pretty text-muted-foreground">
                  Not the treasury? Pay the same transfer from your own wallet and mark that transaction paid to see
                  verification fail.
                </p>
                <Button type="button" variant="secondary" className={TOUCH} disabled={pending} onClick={() => pay.mutate(transfer)}>
                  {pay.isPending ? "Confirm in your wallet…" : "Pay it from my wallet instead"}
                </Button>
              </div>
            </>
          )}
          {refund.status === "pending" && refund.transaction_hash !== null && (
            <p role="status" className="text-sm">
              Marked paid with <ExplorerLink chainId={chainId} kind="tx" value={refund.transaction_hash} />. Verifying once
              the transaction is final; the amount stays reserved meanwhile.
            </p>
          )}
          {refund.status === "succeeded" && (
            <Alert variant="success">
              <CircleCheck aria-hidden="true" />
              <AlertTitle>Refund verified</AlertTitle>
              <AlertDescription>
                The service verified the treasury's transfer at finality and sent{" "}
                <code className="font-mono text-[13px]">deposit.refunded</code>, which took the refunded share back
                from the balance.
              </AlertDescription>
            </Alert>
          )}
          {refund.status === "failed" && (
            <Alert variant="destructive" role="status">
              <CircleAlert aria-hidden="true" />
              <AlertTitle>Refund failed verification</AlertTitle>
              <AlertDescription>
                <code className="font-mono text-[13px]">{refund.failure_reason}</code>: {refund.failure_explanation} Its
                reservation of the deposit is released; declare a new refund and pay it from the treasury.
              </AlertDescription>
            </Alert>
          )}
          {refund.status === "canceled" && (
            <p className="text-sm text-muted-foreground">No payment was attached before it was canceled.</p>
          )}
          {error !== null && <ErrorAlert text={error} />}
          {pay.isError && <ErrorAlert text={errorMessage(pay.error, "The wallet did not send it.")} />}
        </CollapsibleContent>
      </Collapsible>
    </li>
  );
}

function ErrorAlert({ text }: { text: string }) {
  return (
    <Alert variant="destructive">
      <CircleAlert aria-hidden="true" />
      <AlertDescription>{text}</AlertDescription>
    </Alert>
  );
}
