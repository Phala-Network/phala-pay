import * as React from "react"
import { cva, type VariantProps } from "class-variance-authority"
import { cn } from "cn"

/**
 * A status, as a 6px dot in its tone's colour beside text in the foreground colour, which keeps
 * the text's contrast whatever the tone.
 */
const statusBadgeVariants = cva(
  "inline-flex h-6 w-fit shrink-0 items-center gap-1.5 rounded-full border px-2 text-xs font-medium whitespace-nowrap text-foreground before:size-1.5 before:shrink-0 before:rounded-full",
  {
    variants: {
      tone: {
        neutral: "before:bg-muted-foreground",
        success: "before:bg-success",
        warning: "before:bg-warning",
        danger: "before:bg-destructive",
      },
    },
    defaultVariants: {
      tone: "neutral",
    },
  }
)

type StatusTone = NonNullable<VariantProps<typeof statusBadgeVariants>["tone"]>

function StatusBadge({
  className,
  tone = "neutral",
  ...props
}: React.ComponentProps<"span"> & VariantProps<typeof statusBadgeVariants>) {
  return (
    <span
      data-slot="status-badge"
      data-tone={tone}
      className={cn(statusBadgeVariants({ tone }), className)}
      {...props}
    />
  )
}

export { StatusBadge, type StatusTone }
