import * as React from "react"
import { cn } from "cn"

import { CopyButton } from "@/components/ui/copy-button"

/**
 * Code or data (JSON, calldata) as written: monospaced, at its full height, scrolling sideways
 * only where a line is wider than the block (focusable, so the keyboard scrolls it too), with a copy
 * button in its corner. A block in
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
          "overflow-x-auto rounded-lg border bg-muted/50 py-3 pr-12 pl-3 font-mono text-xs leading-relaxed",
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
