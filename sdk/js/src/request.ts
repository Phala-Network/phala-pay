/** Compose caller cancellation with a finite deadline, including response-body reads. */
export function requestSignal(options: {
  signal?: AbortSignal;
  requestTimeout?: number;
}): AbortSignal {
  const deadline = AbortSignal.timeout(options.requestTimeout ?? 10_000);
  return options.signal === undefined ? deadline : AbortSignal.any([options.signal, deadline]);
}

/** `apiBase` without trailing slashes, in linear time (a `/\/+$/` regex is quadratic). */
export function trimTrailingSlashes(apiBase: string): string {
  let end = apiBase.length;
  while (end > 0 && apiBase[end - 1] === "/") end -= 1;
  return apiBase.slice(0, end);
}
