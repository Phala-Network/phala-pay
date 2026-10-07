import * as React from "react"
import { cn } from "@/lib/utils"

/**
 * A multi-line text field, drawn as Input is: 16px text on phones, where anything smaller makes iOS
 * zoom in on focus, and 14px from md.
 */
function Textarea({ className, ...props }: React.ComponentProps<"textarea">) {
  return (
    <textarea
      data-slot="textarea"
      className={cn(
        "w-full min-w-0 rounded-md border border-input bg-background px-3 py-2 text-base transition-colors placeholder:text-muted-foreground focus-visible:border-ring focus-visible:ring-1 focus-visible:ring-ring focus-visible:outline-hidden disabled:pointer-events-none disabled:cursor-not-allowed disabled:bg-muted disabled:opacity-50 aria-invalid:border-destructive aria-invalid:ring-1 aria-invalid:ring-destructive md:text-sm dark:bg-input/20",
        className
      )}
      {...props}
    />
  )
}

export { Textarea }
