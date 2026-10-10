import { createCn } from "cn/config";

/**
 * The site's font sizes beyond Tailwind's (src/index.css, `--text-*`). Class merging must know them
 * as sizes: otherwise `text-mono` reads as a colour and a later `text-foreground` removes it.
 */
export const FONT_SIZES = ["display", "display-sm", "title", "title-sm", "heading", "lead", "lead-lg", "eyebrow", "mono", "table", "wordmark"];

/** Class names joined and merged, Tailwind-aware (cn, tailwind-merge's rules), the site's sizes included. */
export const cn = createCn({
  extend: { classGroups: { "font-size": [{ text: FONT_SIZES }] } },
});
