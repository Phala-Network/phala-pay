import { ConfigurationError, ResponseValidationError } from "./errors.js";
import type { RequestOptions } from "./transport.js";
export interface Execute {
  <T>(
    operation: string,
    method: string,
    path: string,
    params: unknown,
    options?: RequestOptions,
  ): Promise<T>;
}
export function encodeId(id: string): string {
  if (typeof id !== "string" || !id || id === "." || id === "..")
    throw new ConfigurationError("Invalid resource ID");
  try {
    return encodeURIComponent(id);
  } catch {
    throw new ConfigurationError("Invalid resource ID");
  }
}
export async function* paginate<
  T extends { readonly id: string },
  P extends { readonly starting_after?: string },
>(
  page: (params: P) => Promise<{ readonly data: readonly T[]; readonly has_more: boolean }>,
  params: P,
): AsyncIterable<T> {
  let query = { ...params };
  const seen = new Set<string>();
  if (params.starting_after) seen.add(params.starting_after);
  for (;;) {
    const result = await page(query);
    const cursor = result.data.at(-1)?.id;
    if (result.has_more && (!cursor || seen.has(cursor)))
      throw new ResponseValidationError("Invalid pagination cursor");
    for (const item of result.data) yield item;
    if (!result.has_more) return;
    if (cursor === undefined) throw new ResponseValidationError("Invalid pagination cursor");
    seen.add(cursor);
    query = { ...query, starting_after: cursor };
  }
}
