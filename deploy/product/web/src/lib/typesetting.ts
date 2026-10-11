/**
 * The runs of copy that read as one unit, each kept on one line (`whitespace-nowrap`, or
 * `.unbroken` in rendered markdown): a hyphenated word ("ERC-20", "five-minute", "Apache-2.0",
 * "@phala/pay-react", "--pp-color-text"; browsers may otherwise break after a hyphen) or a range
 * ("1–128"), the product's name ("Phala / Pay"), and a number with its unit or symbol ("~7 s",
 * "80 PHA", "15 min"). Spaces elsewhere still break, and so does a unit longer than
 * LONGEST_UNBROKEN, which a narrow column (a comparison's cell on a phone) cannot hold on one line.
 */
const UNITS = /((?:-{0,2}[\p{L}\p{N}@/.~$]+(?:[-–][\p{L}\p{N}.$]+)+)|Phala Pay|~?\$?\d[\d,.]*\s(?:s|ms|min|h|%|PHA|USDC|USDT|ETH)(?![\p{L}\p{N}]))/u;

/** The longest run kept on one line: 16 characters fit the narrowest column the site sets text in. */
export const LONGEST_UNBROKEN = 16;

/**
 * `text` split around its units: the parts at odd indices are units (none longer than
 * LONGEST_UNBROKEN), the rest the text between.
 */
export function splitUnits(text: string): string[] {
  const parts = text.split(UNITS);
  const result: string[] = [parts[0] ?? ""];
  for (let index = 1; index < parts.length; index += 2) {
    const unit = parts[index] ?? "";
    const after = parts[index + 1] ?? "";
    if (unit.length <= LONGEST_UNBROKEN) result.push(unit, after);
    else result.push(`${result.pop() ?? ""}${unit}${after}`);
  }
  return result;
}

/**
 * An identifier or a URL (`payment_settings_unconfirmed`, `/v1/quotes/{id}`) in the parts it may
 * break between: after each run of separators (`_`, `.`, `/`, `:`, `?`, `&`, `=`, `-`, `,`, `|`)
 * that follows a character of the name (not a leading `--` or `/`), and in a long camel-case name
 * (`TopupDeliveredCreditSanctioned`) before each capital. Joined, the parts are the text.
 */
export function splitIdentifier(text: string): string[] {
  return text.split(/(?<=[^_./:?&=,|-][_./:?&=,|-]+)(?=[^_./:?&=,|-])|(?<=[a-z]{3})(?=[A-Z][a-z])/);
}
