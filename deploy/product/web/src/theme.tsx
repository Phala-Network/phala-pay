import { Moon, Sun } from "lucide-react";
import { useSyncExternalStore } from "react";

export type Theme = "light" | "dark";

const listeners = new Set<() => void>();
function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}
function currentTheme(): Theme {
  return document.documentElement.classList.contains("dark") ? "dark" : "light";
}
function setTheme(next: Theme) {
  document.documentElement.classList.toggle("dark", next === "dark");
  try {
    localStorage.setItem("demo-theme", next);
  } catch {
    // Theme changes still work for this visit when persistence is unavailable.
  }
  for (const listener of listeners) listener();
}

/** One theme store shared by the header and demo islands. The head script initializes it. */
export function useTheme(): [Theme, (theme: Theme) => void] {
  return [useSyncExternalStore(subscribe, currentTheme, () => "light" as const), setTheme];
}

/** The header's icon buttons: 36px, with a 44px hit area; one hover and fill in either theme. */
export const ICON_BUTTON =
  "relative inline-flex size-9 shrink-0 items-center justify-center rounded-md text-muted-foreground transition-colors before:absolute before:-inset-1 hover:bg-muted hover:text-foreground focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring [&_svg]:size-4";

/**
 * A toggle for the dark theme. Its icon follows the theme class the head script sets, so it is right
 * before hydration; its pressed state follows the theme store once hydrated.
 */
export function ThemeToggle({ theme, onChange }: { theme: Theme; onChange: (theme: Theme) => void }) {
  return (
    <button type="button" className={ICON_BUTTON} aria-label="Dark theme" aria-pressed={theme === "dark"}
      onClick={() => onChange(theme === "dark" ? "light" : "dark")}>
      <Sun aria-hidden="true" strokeWidth={1.75} className="hidden dark:block" />
      <Moon aria-hidden="true" strokeWidth={1.75} className="dark:hidden" />
    </button>
  );
}
