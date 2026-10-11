import { Fragment, type ReactNode } from "react";
import { splitIdentifier, splitUnits } from "@/lib/typesetting";

/** Copy with its units (src/lib/typesetting.ts: "ERC-20", "Phala Pay", "~7 s") each kept on one line. */
export function unbroken(text: string): ReactNode {
  return splitUnits(text).map((part, index) =>
    index % 2 === 1 ? <span key={index} className="whitespace-nowrap">{part}</span> : part);
}

/** An identifier or a URL that breaks only after its separators (src/lib/typesetting.ts), never mid-word. */
export function divided(text: string): ReactNode {
  return splitIdentifier(text).map((part, index) => <Fragment key={index}>{index > 0 && <wbr />}{part}</Fragment>);
}
