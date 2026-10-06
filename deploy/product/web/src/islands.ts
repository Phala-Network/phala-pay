import { useSyncExternalStore } from "react";

// React's identifierPrefix must match for each independently rendered and hydrated root.
export const ISLAND_PREFIXES = { header: "site-header-", footer: "site-footer-", demo: "demo-" };

const subscribeToHydration = () => () => undefined;
const clientSnapshot = () => true;
const serverSnapshot = () => false;

/** React uses the server snapshot until the prerendered island has hydrated. */
export function useHydrated(): boolean {
  return useSyncExternalStore(subscribeToHydration, clientSnapshot, serverSnapshot);
}
