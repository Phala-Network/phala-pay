import { sources, type Cell } from "./content/compare.js";
import { unbroken } from "./text.js";

/**
 * A comparison value as its source states it: "(partial)" where the vendor states it only in part,
 * "—" where it is not stated. On /compare, with its source's number linked to the list of sources.
 */
export function ComparisonCell({ cell, linkSource }: { cell: Cell; linkSource: boolean }) {
  if (cell.status === "not-stated") {
    return linkSource ? <span aria-describedby="not-stated-note">—</span> : <span className="text-muted-foreground">Not stated</span>;
  }
  const sourceNumber = sources.findIndex(({ url }) => url === cell.source) + 1;
  const text = cell.text.trimEnd();
  const partial = cell.status === "partially";
  // The source number never starts a line of its own: it stays with the "(partial)" note, or
  // without one, with the text's last word. The note itself may wrap to the next line.
  const lastWord = partial ? "" : text.slice(text.lastIndexOf(" ") + 1);
  return (
    <>
      {unbroken(text.slice(0, text.length - lastWord.length))}
      {partial && " "}
      <span className="whitespace-nowrap">
        {unbroken(lastWord)}
        {partial && <span className="text-body-foreground">(partial)</span>}
        {linkSource && sourceNumber > 0 && (
          <sup className="ml-0.5">
            {/* Padding widens the target to about 24px without raising the line: an inline box's
                padding takes clicks but no layout. */}
            <a href={`#source-${sourceNumber}`} aria-label={`Source ${sourceNumber}`}
              className="rounded-sm px-1 py-1.5 text-xs text-body-foreground tabular-nums underline decoration-foreground/30 underline-offset-2 hover:text-foreground hover:decoration-foreground">
              {sourceNumber}
            </a>
          </sup>
        )}
      </span>
    </>
  );
}
