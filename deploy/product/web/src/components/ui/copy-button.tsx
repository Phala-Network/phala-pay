import * as React from "react"
import { Check, Copy, X } from "lucide-react"
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

export { CopyButton }
