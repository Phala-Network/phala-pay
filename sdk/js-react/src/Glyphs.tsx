import type { ReactNode } from "react";

/** A 1em interface glyph drawn in `currentColor`; always decorative. */
function Glyph({ className, children }: { className?: string; children: ReactNode }) {
  return (
    <svg className={className} xmlns="http://www.w3.org/2000/svg" width="1em" height="1em" viewBox="0 0 24 24"
      fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round"
      aria-hidden="true" focusable="false">
      {children}
    </svg>
  );
}

export function CopyGlyph() {
  return <Glyph><rect x="8" y="8" width="12" height="12" rx="2" /><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2" /></Glyph>;
}

export function CheckGlyph() {
  return <Glyph><path d="m5 12.5 4.5 4.5L19 7.5" /></Glyph>;
}

export function CrossGlyph() {
  return <Glyph><path d="m7 7 10 10M17 7 7 17" /></Glyph>;
}

export function ExternalGlyph() {
  return <Glyph className="pp-external"><path d="M7 17 17 7M9 7h8v8" /></Glyph>;
}

/** The status row's tone: a dot while in progress, a check once credited, an alert on failure. */
export function ToneGlyph({ tone }: { tone: "neutral" | "success" | "danger" }) {
  return (
    <Glyph>
      {tone === "neutral" ? (
        <circle cx="12" cy="12" r="4" fill="currentColor" stroke="none" />
      ) : (
        <>
          <circle cx="12" cy="12" r="9" />
          {tone === "success" ? <path d="m8.5 12.5 2.5 2.5 4.5-5" /> : <path d="M12 7.5v5.5M12 16.5h.01" />}
        </>
      )}
    </Glyph>
  );
}
