import * as React from "react"
import { ArrowUpRight, Check, Copy, X } from "lucide-react"
import { cn } from "cn"

/**
 * Copies `value`, confirming with a tick for a moment (announced too). 32px, with a 44px hit area
 * around it.
 */
function CopyButton({
  value,
  label,
  className,
}: {
  value: string
  label: string
  className?: string | undefined
}) {
  const [result, setResult] = React.useState<"copied" | "failed" | null>(null)
  const timer = React.useRef<ReturnType<typeof setTimeout> | undefined>(undefined)
  React.useEffect(() => () => clearTimeout(timer.current), [])
  const show = (outcome: "copied" | "failed") => {
    setResult(outcome)
    clearTimeout(timer.current)
    timer.current = setTimeout(() => setResult(null), 1500)
  }
  return (
    <>
      <button
        type="button"
        data-slot="copy-button"
        aria-label={label}
        className={cn(
          "relative inline-flex size-8 shrink-0 items-center justify-center rounded-md text-muted-foreground transition-colors before:absolute before:-inset-1.5 hover:bg-muted hover:text-foreground focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring [&_svg]:size-4",
          className
        )}
        onClick={() => {
          navigator.clipboard.writeText(value).then(
            () => show("copied"),
            () => show("failed")
          )
        }}
      >
        {result === "copied" ? (
          <Check aria-hidden="true" />
        ) : result === "failed" ? (
          <X aria-hidden="true" />
        ) : (
          <Copy aria-hidden="true" />
        )}
      </button>
      <span className="sr-only" aria-live="polite">
        {result === "copied" ? "Copied" : result === "failed" ? "Could not copy" : ""}
      </span>
    </>
  )
}

/**
 * The page's one way to show an address, hash, id, or key: in full, in groups of four characters,
 * as the SDK shows a payer's address (@phala/pay-react). A type prefix (`0x`, `dep_`, `order_`,
 * `demo-`) stays with the first group. It wraps only between groups, and a group is never shorter
 * than four characters (a remainder joins the last group), so no character is ever left alone on a
 * line. Selecting or copying it gives the value without spaces.
 */
function groups(value: string): string[] {
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
  return groups(value).map((group, index) => (
    <React.Fragment key={index}>
      {index > 0 && <wbr />}
      <span className="mr-[0.5ch] whitespace-nowrap last:mr-0">{group}</span>
    </React.Fragment>
  ))
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
  const text = (
    <span data-slot="hash-value" className="font-mono text-[0.8125rem] text-foreground">
      <Grouped value={value} />
    </span>
  )
  return (
    // With a copy button: the value and the button in one row, the value wrapping beside it.
    <span
      data-slot="hash"
      data-value={value}
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
        <CopyButton value={value} label={copyLabel} className="-my-1.5 shrink-0" />
      )}
    </span>
  )
}

export { CopyButton, Hash }
