import * as React from "react"
import { cn } from "cn"

import { CopyButton } from "@/components/ui/hash"

/**
 * Code or data (JSON, calldata) as written: monospaced, at most 240px tall and scrolling beyond
 * (focusable, so the keyboard scrolls it too), with a copy button that stays in its corner.
 */
function CodeBlock({
  value,
  label,
  className,
}: {
  value: string
  /** What the block holds, for its copy button: "Copy {label}". */
  label: string
  className?: string | undefined
}) {
  return (
    <div data-slot="code-block" className="relative min-w-0">
      <pre
        tabIndex={0}
        className={cn(
          "max-h-60 overflow-auto rounded-lg border bg-muted/50 py-3 pr-12 pl-3 font-mono text-xs leading-relaxed",
          className
        )}
      >
        <code>{value}</code>
      </pre>
      <CopyButton value={value} label={`Copy ${label}`} className="absolute top-1.5 right-1.5" />
    </div>
  )
}

export { CodeBlock }
