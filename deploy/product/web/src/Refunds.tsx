import { useMutation } from "@tanstack/react-query";
import { CircleAlert } from "lucide-react";
import { useId, useState, type FormEvent } from "react";
import { isAddress, isHash, parseUnits } from "viem";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Field, FieldLabel } from "@/components/ui/field";
import { Hash } from "@/components/ui/hash";
import { Input } from "@/components/ui/input";
import { StatusBadge } from "@/components/ui/status-badge";
import type { Deposit, Refund, Timeline } from "./api.js";
import { assetOf, networkOf } from "./chains.js";
import { Detail, Details, ExplorerLink, InfoTip, Subsection, describe, errorMessage, statusTone, wallet } from "./common.js";
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
    <Subsection
      title={`Refunds (${timeline.refunds.length})`}
      id="refunds-title"
      aside={
        <InfoTip label="About refunds">
          The merchant refunds from its own treasury: declare the refund, pay it from the treasury that this
          deposit's address pays, then attach the transaction. Phala Pay verifies it once the transaction is final
          and never moves funds. Here you play the merchant's finance team.
        </InfoTip>
      }
    >
      <p className="text-muted-foreground">
        On this staging demo the treasury{" "}
        <ExplorerLink chainId={token.chainId} kind="address" value={treasury} /> is Phala's finance Safe, which
        you do not control: a refund you pay from your own wallet is verified and <strong>fails</strong> with{" "}
        <code>sender_mismatch</code>, which is exactly what should happen.
      </p>
      {refundable ? (
        <RefundForm deposit={deposit} token={token} />
      ) : (
        <p data-testid="refund-unavailable" className="text-muted-foreground">
          {deposit.status === "reversed"
            ? "A reversed deposit cannot be refunded."
            : "Refunds need a final deposit (the service answers 400 deposit_not_final before)."}
        </p>
      )}
      {timeline.refunds.length > 0 && (
        <ul className="flex flex-col divide-y border-t" aria-label="Refunds of this deposit">
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

function RefundForm({ deposit, token }: { deposit: Deposit; token: RefundToken }) {
  const { symbol, decimals } = token;
  const remaining = BigInt(deposit.amount_atomic) - BigInt(deposit.amount_refunded_atomic);
  const [amount, setAmount] = useState("");
  const [destination, setDestination] = useState(deposit.from_address);
  const [invalid, setInvalid] = useState<string | null>(null);
  const create = useCreateRefund();
  const amountId = useId();
  const destinationId = useId();
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
    if (!isAddress(destination)) {
      setInvalid("Enter a 0x address for the destination.");
      return;
    }
    setInvalid(null);
    create.mutate(
      { deposit: deposit.id, amountAtomic: atomic.toString(), destinationAddress: destination },
      { onSuccess: () => setAmount("") },
    );
  };
  const error = invalid ?? (create.error === null ? null : `Could not declare the refund: ${describe(create.error)}.`);
  return (
    <form className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_minmax(0,1.6fr)]" onSubmit={submit} aria-label="Declare a refund">
      <Field>
        <FieldLabel htmlFor={amountId}>
          Amount ({symbol}, at most {tokens(remaining.toString(), symbol, decimals)})
        </FieldLabel>
        <Input
          id={amountId}
          inputMode="decimal"
          placeholder="10"
          value={amount}
          onChange={(event) => setAmount(event.target.value)}
        />
      </Field>
      <Field>
        <FieldLabel htmlFor={destinationId}>Destination address (the payer's, by default)</FieldLabel>
        <Input
          id={destinationId}
          className="font-mono"
          spellCheck={false}
          value={destination}
          onChange={(event) => setDestination(event.target.value)}
        />
      </Field>
      <Button type="submit" variant="secondary" className="justify-self-start sm:col-span-2" disabled={create.isPending}>
        {create.isPending ? "Declaring…" : "Declare refund"}
      </Button>
      {error !== null && (
        <div className="sm:col-span-2">
          <ErrorAlert text={error} />
        </div>
      )}
    </form>
  );
}

function RefundItem({ refund, token }: { refund: Refund; token: RefundToken }) {
  const { chainId, symbol, decimals } = token;
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
    <li
      className="flex flex-col gap-3 py-4"
      data-testid="refund"
      data-refund={refund.id}
      data-status={refund.status}
    >
      <div className="flex items-center justify-between gap-2">
        <Hash value={refund.id} className="text-muted-foreground" />
        <StatusBadge tone={statusTone(refund.status)}>{statusLabel(refund.status)}</StatusBadge>
      </div>
      <p>
        {tokens(refund.amount_atomic, symbol, decimals)} to{" "}
        <ExplorerLink chainId={chainId} kind="address" value={refund.destination_address} />
      </p>
      {transfer !== null && (
        <>
          <div className="flex flex-col gap-2" data-testid="refund-transfer">
            <p>
              <strong>Pay exactly this transfer from the treasury</strong>, then attach its hash:
            </p>
            <Details>
              <Detail label="From (treasury)" className="font-mono">
                {transfer.from}
              </Detail>
              <Detail label="Token contract" className="font-mono">
                {transfer.token}
              </Detail>
              <Detail label="To" className="font-mono">
                {transfer.to}
              </Detail>
              <Detail label="Amount">
                {tokens(transfer.amount_atomic, symbol, decimals)} (<span className="font-mono">{transfer.amount_atomic}</span>)
              </Detail>
              <Detail label="Calldata" className="font-mono">
                {transfer.data}
              </Detail>
            </Details>
          </div>
          <form className="flex flex-col gap-3" onSubmit={markPaid} aria-label={`Mark refund ${refund.id} paid`}>
            <Field>
              <FieldLabel htmlFor={hashId}>Transaction hash of the payment</FieldLabel>
              <Input
                id={hashId}
                className="font-mono"
                spellCheck={false}
                placeholder="0x…"
                value={hash}
                onChange={(event) => setHash(event.target.value)}
              />
            </Field>
            <Field>
              <FieldLabel htmlFor={indexId}>
                Receipt log index (optional, when one transaction pays several refunds)
              </FieldLabel>
              <Input
                id={indexId}
                inputMode="numeric"
                value={logIndex}
                onChange={(event) => setLogIndex(event.target.value)}
              />
            </Field>
            <div className="flex flex-wrap gap-2">
              <Button type="submit" variant="secondary" disabled={pending}>
                {mark.isPending ? "Submitting…" : "Mark paid"}
              </Button>
              <Button type="button" variant="ghost" disabled={pending} onClick={() => cancel.mutate(refund.id)}>
                {cancel.isPending ? "Canceling…" : "Cancel refund"}
              </Button>
            </div>
          </form>
          <p className="text-muted-foreground">
            Not the treasury?{" "}
            <Button
              type="button"
              variant="link"
              className="h-auto p-0 text-xs text-foreground underline"
              disabled={pending}
              onClick={() => pay.mutate(transfer)}
            >
              {pay.isPending ? "Confirm in your wallet…" : "Pay it from my wallet instead"}
            </Button>{" "}
            and mark that transaction paid to see verification fail.
          </p>
        </>
      )}
      {refund.status === "pending" && refund.transaction_hash !== null && (
        <p role="status">
          Marked paid with <ExplorerLink chainId={chainId} kind="tx" value={refund.transaction_hash} />. Verifying once
          the transaction is final; the amount stays reserved meanwhile.
        </p>
      )}
      {refund.status === "succeeded" && (
        <p role="status">
          The service verified the treasury's transfer at finality and sent <code>deposit.refunded</code>, which took the
          refunded share back from the balance.
        </p>
      )}
      {refund.status === "failed" && (
        <Alert variant="destructive" role="status">
          <CircleAlert aria-hidden="true" />
          <AlertTitle>
            <code>{refund.failure_reason}</code>
          </AlertTitle>
          <AlertDescription>
            {refund.failure_explanation} Its reservation of the deposit is released; declare a new refund and pay it from
            the treasury.
          </AlertDescription>
        </Alert>
      )}
      {refund.status === "canceled" && <p className="text-muted-foreground">No payment was attached before it was canceled.</p>}
      {error !== null && <ErrorAlert text={error} />}
      {pay.isError && <ErrorAlert text={errorMessage(pay.error, "The wallet did not send it.")} />}
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
