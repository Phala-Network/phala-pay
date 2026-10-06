import * as React from "react"
import { cn } from "cn"

import { Label } from "@/components/ui/label"

/** A form field: its label above its control, 8px apart. */
function Field({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      role="group"
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
