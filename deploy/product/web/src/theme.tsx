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

/** The header's icon buttons: one hover and fill in either theme. */
export const ICON_BUTTON =
  "inline-flex size-10 shrink-0 items-center justify-center rounded-md text-muted-foreground transition-colors focus-visible:outline-hidden hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring [&_svg]:size-4";

export function ThemeToggle({ theme, onChange }: { theme: Theme; onChange: (theme: Theme) => void }) {
  const next = theme === "dark" ? "light" : "dark";
  return (
    <button type="button" className={ICON_BUTTON} onClick={() => onChange(next)}>
      <Sun aria-hidden="true" className="hidden dark:block" />
      <Moon aria-hidden="true" className="dark:hidden" />
      <span className="sr-only hidden dark:block">Switch to light theme</span>
      <span className="sr-only dark:hidden">Switch to dark theme</span>
    </button>
  );
}
