import * as React from "react"
import { cn } from "@/lib/utils"

import { Input } from "@/components/ui/input"

/** A 40px text field with a prefix (a unit, a currency sign) inside its border. */
function InputGroup({ className, ...props }: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="input-group"
      role="group"
      className={cn(
        "group/input-group relative flex h-10 w-full min-w-0 items-center rounded-md border border-input bg-background transition-colors has-disabled:bg-muted has-disabled:opacity-50 has-[[data-slot=input-group-control]:focus-visible]:border-ring has-[[data-slot=input-group-control]:focus-visible]:ring-1 has-[[data-slot=input-group-control]:focus-visible]:ring-ring has-[[data-slot][aria-invalid=true]]:border-destructive has-[[data-slot][aria-invalid=true]]:ring-1 has-[[data-slot][aria-invalid=true]]:ring-destructive dark:bg-input/20",
        className
      )}
      {...props}
    />
  )
}

/** The prefix: clicking it focuses the field. */
function InputGroupAddon({
  className,
  ...props
}: React.ComponentProps<"div">) {
  return (
    <div
      data-slot="input-group-addon"
      className={cn(
        "order-first flex h-auto cursor-text items-center justify-center gap-2 py-1.5 pl-3 text-sm font-medium text-muted-foreground select-none [&>svg:not([class*='size-'])]:size-4",
        className
      )}
      onClick={(event) => {
        event.currentTarget.parentElement?.querySelector("input")?.focus()
      }}
      {...props}
    />
  )
}

function InputGroupText({ className, ...props }: React.ComponentProps<"span">) {
  return (
    <span
      className={cn(
        "flex items-center gap-2 text-sm text-muted-foreground [&_svg]:pointer-events-none [&_svg:not([class*='size-'])]:size-4",
        className
      )}
      {...props}
    />
  )
}

function InputGroupInput({
  className,
  ...props
}: React.ComponentProps<"input">) {
  return (
    <Input
      data-slot="input-group-control"
      className={cn(
        "flex-1 rounded-none border-0 bg-transparent pl-1.5 ring-0 focus-visible:ring-0 aria-invalid:ring-0 disabled:bg-transparent dark:bg-transparent",
        className
      )}
      {...props}
    />
  )
}

export { InputGroup, InputGroupAddon, InputGroupText, InputGroupInput }
