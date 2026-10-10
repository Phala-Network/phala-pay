// The page's one layout grid, shared by the site's sections (src/Site.tsx) and the demo
// (src/Demo.tsx), which loads on its own.

/**
 * 12 columns with 32px gutters from lg, inside the page's container. A part in two columns marks
 * them `data-column="left"` and `data-column="right"`, and its grid names its layout
 * (`data-layout`): every right column of one layout starts on one line (e2e/demo.spec.ts measures
 * it). Below lg, the columns stack.
 */
export const GRID = "grid lg:grid-cols-12 lg:gap-x-8";

/** The split (`data-layout="split"`): halves, the right one from the 7th column. */
export const LEFT = "min-w-0 lg:col-span-6";
export const RIGHT = "min-w-0 lg:col-span-6 lg:col-start-7";

/**
 * The aside (`data-layout="aside"`): a third for a heading or the brand, two thirds for what it
 * introduces (the FAQ's questions, the footer's links), from the 5th column.
 */
export const ASIDE = "min-w-0 lg:col-span-4";
export const MAIN = "min-w-0 lg:col-span-8 lg:col-start-5";
