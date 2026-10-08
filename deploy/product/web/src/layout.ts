// The page's one layout grid, shared by the site's sections (src/Site.tsx) and the demo
// (src/Demo.tsx), which loads on its own.

/**
 * 12 columns with 32px gutters from lg, inside the page's container. A section in two columns puts
 * its heading or main content in LEFT and the other in RIGHT, which always starts at the 7th
 * column: every right column on the page starts on one line (e2e/demo.spec.ts measures it, by
 * `data-column="right"`). Below lg, the columns stack.
 */
export const GRID = "grid lg:grid-cols-12 lg:gap-x-8";
/** The left half: columns 1–6. */
export const LEFT = "min-w-0 lg:col-span-6";
/** The right half: columns 7–12. */
export const RIGHT = "min-w-0 lg:col-span-6 lg:col-start-7";
