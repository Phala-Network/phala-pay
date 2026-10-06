import * as React from "react"
import { cn } from "cn"

/**
 * A radio choice with a description, laid out as a card around its RadioGroupItem. Selection and
 * keyboard focus show independently, and together: the chosen card's border is 2px of the primary
 * colour (its border and an inset ring), the focused card has the focus outline outside it.
 */
function ChoiceCard({ className, ...props }: React.ComponentProps<"label">) {
  return (
    <label
      data-slot="choice-card"
      className={cn(
        "flex min-w-0 cursor-pointer items-center gap-3 rounded-lg border bg-card px-3 py-2.5 text-sm transition-colors hover:bg-muted/50 has-data-checked:border-primary has-data-checked:ring-1 has-data-checked:ring-primary has-data-checked:ring-inset has-focus-visible:outline-2 has-focus-visible:outline-offset-2 has-focus-visible:outline-ring has-disabled:cursor-not-allowed has-disabled:opacity-50",
        className
      )}
      {...props}
    />
  )
}

export { ChoiceCard }
