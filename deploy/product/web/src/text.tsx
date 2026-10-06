import type { ReactNode } from "react";

/** Copy with "ERC-20" kept on one line: browsers may otherwise break after its hyphen. */
export function unbroken(text: string): ReactNode {
  return text.split(/(ERC-20)/).map((part, index) =>
    index % 2 === 1 ? <span key={index} className="whitespace-nowrap">{part}</span> : part);
}
