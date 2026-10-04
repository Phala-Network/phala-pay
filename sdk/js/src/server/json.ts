/** Parse before number conversion: JSON.parse alone silently rounds int64 responses. */
export function parseJson(text: string): unknown {
  let at = 0;
  const fail = (): never => {
    throw new SyntaxError("Invalid JSON");
  };
  const space = () => {
    while (/[ \t\r\n]/.test(text[at] ?? "") && at < text.length) at++;
  };
  const string = (): string => {
    const start = at++;
    while (at < text.length) {
      const char = text[at++];
      if (char === '"') return JSON.parse(text.slice(start, at)) as string;
      if (char === "\\") at++;
    }
    return fail();
  };
  const value = (): unknown => {
    space();
    const char = text[at];
    if (char === '"') return string();
    if (char === "{" || char === "[") {
      at++;
      const array = char === "[";
      const result: Record<string, unknown> = Object.create(null) as Record<string, unknown>;
      const items: unknown[] = [];
      space();
      if (text[at] === (array ? "]" : "}")) {
        at++;
        return array ? items : result;
      }
      for (;;) {
        space();
        if (array) items.push(value());
        else {
          if (text[at] !== '"') fail();
          const key = string();
          space();
          if (text[at++] !== ":" || Object.hasOwn(result, key)) fail();
          result[key] = value();
        }
        space();
        const next = text[at++];
        if (next === (array ? "]" : "}")) return array ? items : result;
        if (next !== ",") fail();
      }
    }
    for (const [token, decoded] of [
      ["true", true],
      ["false", false],
      ["null", null],
    ] as const) {
      if (text.startsWith(token, at)) {
        at += token.length;
        return decoded;
      }
    }
    const match = /^-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?/.exec(text.slice(at));
    if (!match) return fail();
    at += match[0].length;
    const number = Number(match[0]);
    // Integer tokens are compared in arbitrary precision before conversion.
    if (!Number.isFinite(number) || (Number.isInteger(number) && !Number.isSafeInteger(number)))
      fail();
    if (
      /^-?\d+$/.test(match[0]) &&
      (BigInt(match[0]) > BigInt(Number.MAX_SAFE_INTEGER) ||
        BigInt(match[0]) < BigInt(Number.MIN_SAFE_INTEGER))
    )
      fail();
    if (Number.isInteger(number)) {
      const [mantissa = "", exponent = "0"] = match[0].toLowerCase().split("e");
      const fraction = mantissa.split(".")[1]?.length ?? 0;
      const scale = fraction - Number(exponent);
      const exact = BigInt(mantissa.replace(".", ""));
      if (Math.abs(scale) > 400) {
        if (exact !== 0n) fail();
      } else if (
        scale >= 0
          ? BigInt(number) * 10n ** BigInt(scale) !== exact
          : BigInt(number) !== exact * 10n ** BigInt(-scale)
      )
        fail();
    }
    return number;
  };
  const decoded = value();
  space();
  if (at !== text.length) fail();
  return decoded;
}
export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
export function validateNumbers(value: unknown): void {
  if (
    typeof value === "number" &&
    (!Number.isFinite(value) || (Number.isInteger(value) && !Number.isSafeInteger(value)))
  )
    throw new TypeError("Unsafe number");
  if (Array.isArray(value)) value.forEach(validateNumbers);
  else if (isRecord(value)) Object.values(value).forEach(validateNumbers);
}
