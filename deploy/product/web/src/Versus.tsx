import { cn } from "@/lib/utils";
import { ComparisonCell } from "./ComparisonCell.js";
import type { Competitor, DimensionKey } from "./content/compare.js";

/**
 * Each provider's column shows while its radio is checked: native radios and CSS `:has()`, so the
 * choice works in the prerendered page, without script. One literal class per provider, which
 * Tailwind generates.
 */
const SHOWN_WHEN_CHOSEN: Record<string, string> = {
  stripe: "group-has-[[data-vendor=stripe]:checked]/versus:table-cell",
  "coinbase-business": "group-has-[[data-vendor=coinbase-business]:checked]/versus:table-cell",
  btcpay: "group-has-[[data-vendor=btcpay]:checked]/versus:table-cell",
  nowpayments: "group-has-[[data-vendor=nowpayments]:checked]/versus:table-cell",
  "moonpay-commerce": "group-has-[[data-vendor=moonpay-commerce]:checked]/versus:table-cell",
};

/**
 * On a phone, Phala Pay beside one provider at a time, chosen above the table: two columns, a row
 * per dimension under its name. `name` keeps each instance's radios apart.
 */
export function Versus({ phala, others, dimensions, linkSource, name, className }: {
  phala: Competitor;
  others: Competitor[];
  dimensions: { key: DimensionKey; label: string }[];
  linkSource: boolean;
  name: string;
  className?: string;
}) {
  const shown = (id: string) => cn("hidden", SHOWN_WHEN_CHOSEN[id]);
  return (
    <div className={cn("group/versus", className)}>
      <fieldset>
        <legend className="text-sm font-medium text-muted-foreground">Compare Phala Pay with</legend>
        <div className="mt-3 flex flex-wrap gap-2">
          {others.map(({ id, name: label }, index) => (
            <label key={id}
              className="inline-flex min-h-11 cursor-pointer items-center rounded-md border px-3 text-sm font-medium text-body-foreground transition-colors has-checked:border-foreground has-checked:bg-muted has-checked:text-foreground has-focus-visible:outline-2 has-focus-visible:outline-offset-2 has-focus-visible:outline-ring">
              <input type="radio" name={name} value={id} data-vendor={id} defaultChecked={index === 0} className="sr-only" />
              {label}
            </label>
          ))}
        </div>
      </fieldset>
      <table className="mt-6 w-full table-fixed border-collapse text-left text-sm">
        <caption className="sr-only">Phala Pay beside the provider chosen above, a row per dimension.</caption>
        <thead>
          <tr>
            <th scope="col" className="w-1/2 border-b-2 border-brand-ink pr-3 pb-2 font-semibold">{phala.name}</th>
            {others.map(({ id, name: label }) => (
              <th key={id} scope="col" className={cn(shown(id), "border-b-2 pb-2 pl-3 font-semibold text-body-foreground")}>{label}</th>
            ))}
          </tr>
        </thead>
        {dimensions.map(({ key, label }) => (
          <tbody key={key}>
            <tr>
              <th scope="rowgroup" colSpan={2} className="pt-5 pb-1.5 mono-label text-muted-foreground">{label}</th>
            </tr>
            <tr className="border-b">
              <td className="pr-3 pb-3 align-top leading-6 text-foreground"><ComparisonCell cell={phala[key]} linkSource={linkSource} /></td>
              {others.map((vendor) => (
                <td key={vendor.id} className={cn(shown(vendor.id), "pb-3 pl-3 align-top leading-6 text-body-foreground")}>
                  <ComparisonCell cell={vendor[key]} linkSource={linkSource} />
                </td>
              ))}
            </tr>
          </tbody>
        ))}
      </table>
    </div>
  );
}
