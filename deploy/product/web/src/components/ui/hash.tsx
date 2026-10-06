import * as React from "react"
import { Check, Copy, X } from "lucide-react"
import { cn } from "cn"

import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { short } from "@/format"

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
 * A hash, address, or id: monospaced and middle-truncated by the page's one rule (`short`), with
 * the full value in a tooltip; linked when `href` is given, with a copy button when `copyLabel` is.
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
  const text = short(value)
  const shown =
    href === undefined ? (
      <span data-slot="hash-value" className="font-mono text-[13px]">
        {text}
      </span>
    ) : (
      <a
        data-slot="hash-value"
        className="rounded-sm font-mono text-[13px] underline decoration-foreground/30 underline-offset-4 transition-colors hover:decoration-foreground"
        href={href}
        target="_blank"
        rel="noreferrer"
      >
        {text}
      </a>
    )
  return (
    <span
      data-slot="hash"
      className={cn("inline-flex max-w-full items-center gap-1 align-middle", className)}
    >
      {text === value ? (
        shown
      ) : (
        <Tooltip>
          <TooltipTrigger asChild>{shown}</TooltipTrigger>
          <TooltipContent className="font-mono break-all">{value}</TooltipContent>
        </Tooltip>
      )}
      {copyLabel !== undefined && <CopyButton value={value} label={copyLabel} />}
    </span>
  )
}

export { CopyButton, Hash }
