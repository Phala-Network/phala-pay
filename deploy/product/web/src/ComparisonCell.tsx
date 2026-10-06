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
  return (
    <>
      {unbroken(cell.text)}
      {/* No-break spaces keep the note and the source number on the text's last line. */}
      {cell.status === "partially" && <span className="text-muted-foreground">{" "}(partial)</span>}
      {linkSource && sourceNumber > 0 && (
        <>
          {" "}
          <sup>
            {/* Padding widens the target to about 24px without raising the line: an inline box's
                padding takes clicks but no layout. */}
            <a href={`#source-${sourceNumber}`} aria-label={`Source ${sourceNumber}`}
              className="rounded-sm px-1 py-1.5 text-xs text-muted-foreground tabular-nums underline-offset-2 hover:text-foreground hover:underline">
              {sourceNumber}
            </a>
          </sup>
        </>
      )}
    </>
  );
}
