import type { UseQueryResult } from "@tanstack/react-query";
import { cn } from "@/lib/utils";
import { isTerminalApiError } from "./api.js";

export interface QueryView<T> {
  data: T | undefined;
  error: string | null;
  paused: boolean;
}

/** Preserve cached data and distinguish recoverable outages from permanent refusals. */
export function queryView<T>(result: Pick<UseQueryResult<T, unknown>, "data" | "isError" | "error">, subject: string): QueryView<T> {
  const terminal = result.isError && isTerminalApiError(result.error);
  return {
    data: result.data,
    error: result.isError && result.data === undefined
      ? `${subject} unavailable ${terminal ? "for this request." : "right now; retrying…"}`
      : null,
    paused: terminal && result.data !== undefined,
  };
}

/** The shared muted query error or paused-updates line. */
export function QueryState({ view, className }: {
  view: Pick<QueryView<unknown>, "error" | "paused">;
  className?: string;
}) {
  const message = view.error ?? (view.paused ? "Updates paused." : null);
  return message === null ? null : <p className={cn("text-sm text-muted-foreground", className)} role="status">{message}</p>;
}
