import * as React from "react"
import { cn } from "cn"

import { CopyButton } from "@/components/ui/hash"

/**
 * Code or data (JSON, calldata) as written: monospaced, at most 240px tall and scrolling beyond
 * (focusable, so the keyboard scrolls it too), with a copy button that stays in its corner. A block in
 * static HTML passes `copyable={false}` until it hydrates, so it shows no button that cannot work.
 */
function CodeBlock({
  value,
  label,
  copyable = true,
  className,
}: {
  value: string
  /** What the block holds: its name, and its copy button's ("Copy {label}"). */
  label: string
  copyable?: boolean
  className?: string | undefined
}) {
  return (
    <div data-slot="code-block" className="relative min-w-0">
      <pre
        tabIndex={0}
        role="region"
        aria-label={label}
        className={cn(
          "max-h-60 overflow-auto rounded-lg border bg-muted/50 py-3 pr-12 pl-3 font-mono text-xs leading-relaxed",
          className
        )}
      >
        <code>{value}</code>
      </pre>
      {copyable && (
        <CopyButton value={value} label={`Copy ${label}`} className="absolute top-1.5 right-1.5" />
      )}
    </div>
  )
}

export { CodeBlock }
