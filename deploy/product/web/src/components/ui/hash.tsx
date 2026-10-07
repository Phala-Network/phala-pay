import * as React from "react"
import { ArrowUpRight } from "lucide-react"
import { cn } from "cn"
import { getAddress, isAddress } from "viem"

import { CopyButton } from "@/components/ui/copy-button"

/**
 * The page's one way to show an address, hash, or id: in full, in groups of four characters, as the
 * SDK shows a payer's address (@phala/pay-react). A type prefix (`0x`, `dep_`, `order_`, `demo-`)
 * stays with the first group. It wraps only between groups, and a group is never shorter than four
 * characters (a remainder joins the last group), so no character is ever left alone on a line.
 * Selecting or copying it gives the value without spaces. Only hex and ids are grouped: a key in
 * base64 (a webhook key) is shown as it is, wrapping anywhere.
 */
function groups(value: string): string[] | null {
  const id = /^(?:0x|[A-Za-z]+[_-])?[0-9A-Za-z]+$/.exec(value)
  if (id === null || value.length < 12) return null
  const prefix = /^(?:0x|[A-Za-z]+[_-])/.exec(value)?.[0].length ?? 0
  const result = [value.slice(0, prefix + 4)]
  for (let start = prefix + 4; start < value.length; start += 4) {
    result.push(value.slice(start, start + 4))
  }
  const last = result.at(-1) ?? ""
  if (result.length > 1 && last.length < 4) {
    result.splice(-2, 2, `${result.at(-2) ?? ""}${last}`)
  }
  return result
}

function Grouped({ value }: { value: string }) {
  const parts = groups(value)
  if (parts === null) return <span className="break-all">{value}</span>
  return parts.map((group, index) => (
    <React.Fragment key={index}>
      {index > 0 && <wbr />}
      <span className="mr-[0.5ch] whitespace-nowrap last:mr-0">{group}</span>
    </React.Fragment>
  ))
}

/** An EVM address in its EIP-55 checksum case, as wallets and explorers show it; anything else as it is. */
function displayed(value: string): string {
  return isAddress(value, { strict: false }) ? getAddress(value) : value
}

/**
 * A hash, address, or id, always in full (never shortened, so it can be compared character by
 * character): monospaced, grouped in fours. Linked when `href` is given, marked by an arrow;
 * with a copy button when `copyLabel` is. Inline, it wraps with the sentence around it.
 */
function Hash({
  value,
  href,
  copyLabel,
  className,
}: {
  value: string
  href?: string | undefined
  copyLabel?: string | undefined
  className?: string | undefined
}) {
  const shown = displayed(value)
  const text = (
    <span data-slot="hash-value" className="font-mono text-mono text-foreground">
      <Grouped value={shown} />
    </span>
  )
  return (
    // With a copy button: the value and the button in one row, the value wrapping beside it.
    <span
      data-slot="hash"
      data-value={shown}
      className={cn(copyLabel === undefined ? "inline" : "inline-flex max-w-full items-start gap-1 align-top", className)}
    >
      {href === undefined ? (
        text
      ) : (
        <a
          // A block of its own lines, padded to a 44px target without moving the text around it.
          className="-my-1.5 inline-block max-w-full rounded-sm py-1.5 decoration-foreground/40 underline-offset-4 hover:underline"
          href={href}
          target="_blank"
          rel="noreferrer"
        >
          {text}
          <ArrowUpRight aria-hidden="true" className="ml-0.5 inline size-3.5 align-[-0.125em] text-muted-foreground" />
          <span className="sr-only"> (opens the block explorer)</span>
        </a>
      )}
      {copyLabel !== undefined && (
        <CopyButton value={shown} label={copyLabel} className="-my-1.5 shrink-0" />
      )}
    </span>
  )
}

export { Hash }
