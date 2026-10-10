import type { ReactNode } from "react";
import type { Token } from "../../scripts/highlight.ts";
import { cn } from "@/lib/utils";

/** Lines of build-time tokens (scripts/highlight.ts) as text, each token's class its colour. */
export function HighlightedLines({ lines }: { lines: Token[][] }): ReactNode {
  return lines.map((line, index) => (
    <span key={index} className="block min-h-[1lh]">
      {line.map((token, at) => (token.className === undefined ? token.text : <span key={at} className={token.className}>{token.text}</span>))}
    </span>
  ));
}

/**
 * The page's code window: one dark surface in either theme, a header row (what it shows, a file name,
 * actions), code that scrolls inside the window, never past the page's edge, and a footer, when
 * there is one, for the line that goes with the code: part of the window, not floating under it.
 */
export function CodeWindow({ header, footer, children, className }: { header: ReactNode; footer?: ReactNode; children: ReactNode; className?: string }) {
  return (
    <div className={cn("dark overflow-hidden rounded-xl border border-code-border bg-code text-code-foreground shadow-frame", className)}>
      <div className="flex h-12 items-center gap-3 border-b border-code-border pr-2 pl-4">{header}</div>
      {children}
      {footer !== undefined && (
        <div className="border-t border-code-border px-4 py-3 text-sm text-pretty text-code-muted sm:px-5">{footer}</div>
      )}
    </div>
  );
}

/** The window's code: 13px Geist Mono, scrolling sideways (focusable, so the keyboard scrolls it too). */
export function CodeBody({ label, children, className }: { label: string; children: ReactNode; className?: string }) {
  return (
    <pre tabIndex={0} role="region" aria-label={label}
      className={cn("overflow-x-auto px-5 py-4 font-mono text-mono text-code-foreground", className)}>
      <code>{children}</code>
    </pre>
  );
}
