import * as React from "react"
import { cn } from "@/lib/utils"

import { Label } from "@/components/ui/label"

/**
 * A form field: its label above its one control, 8px apart. A layout box, not a group: the label
 * names the control, and a group named the same would only repeat it.
 */
function Field({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="field"
      className={cn(
        "group/field flex w-full flex-col gap-2 *:w-full [&>.sr-only]:w-auto",
        className
      )}
      {...props}
    />
  )
}

function FieldLabel({
  className,
  ...props
}: React.ComponentProps<typeof Label>) {
  return (
    <Label
      data-slot="field-label"
      className={cn("flex w-fit gap-2 leading-snug", className)}
      {...props}
    />
  )
}

export { Field, FieldLabel }
