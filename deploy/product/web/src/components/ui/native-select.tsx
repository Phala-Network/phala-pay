import * as React from "react"
import { cn } from "cn"
import { ChevronDownIcon } from "lucide-react"

/**
 * The page's one select: a native `<select>`, which the CSP allows (Radix Select injects a
 * `<style>`) and phones present natively. Its own arrow is hidden for a 16px chevron, 12px in
 * from the right border, with the text kept clear of it.
 */
function NativeSelect({ className, ...props }: React.ComponentProps<"select">) {
  return (
    <div
      data-slot="native-select-wrapper"
      className="group/native-select relative w-full min-w-0 has-[select:disabled]:opacity-50"
    >
      <select
        data-slot="native-select"
        className={cn(
          "h-10 w-full min-w-0 appearance-none rounded-md border border-input bg-background py-0 pr-9 pl-3 text-base transition-colors focus-visible:outline-hidden focus-visible:border-ring focus-visible:ring-1 focus-visible:ring-ring disabled:pointer-events-none disabled:cursor-not-allowed aria-invalid:border-destructive aria-invalid:ring-1 aria-invalid:ring-destructive md:text-sm dark:bg-input/20",
          className
        )}
        {...props}
      />
      <ChevronDownIcon
        data-slot="native-select-icon"
        aria-hidden="true"
        className="pointer-events-none absolute top-1/2 right-3 size-4 -translate-y-1/2 text-muted-foreground"
      />
    </div>
  )
}

function NativeSelectOption(props: React.ComponentProps<"option">) {
  return <option data-slot="native-select-option" {...props} />
}

export { NativeSelect, NativeSelectOption }
