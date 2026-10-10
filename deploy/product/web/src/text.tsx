import type { ReactNode } from "react";

/**
 * Copy with its hyphenated words ("ERC-20", "five-minute") kept on one line (browsers may otherwise
 * break after the hyphen), and the product's name: "Phala / Pay" never splits across lines.
 */
export function unbroken(text: string): ReactNode {
  return text.split(/(ERC-20|five-minute|Phala Pay)/).map((part, index) =>
    index % 2 === 1 ? <span key={index} className="whitespace-nowrap">{part}</span> : part);
}
