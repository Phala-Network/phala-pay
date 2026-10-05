import { Moon, Sun } from "lucide-react";
import { useEffect, useState } from "react";

export type Theme = "light" | "dark";

/** The visitor's theme: stored, else the system's; `dark` on `<html>`. */
export function useTheme(): [Theme, (theme: Theme) => void] {
  const [theme, setTheme] = useState<Theme>(() => {
    try {
      const stored = localStorage.getItem("demo-theme");
      if (stored === "light" || stored === "dark") return stored;
    } catch {
      // Use the system preference when storage is unavailable.
    }
    return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  });
  useEffect(() => {
    document.documentElement.classList.toggle("dark", theme === "dark");
  }, [theme]);
  return [
    theme,
    (next) => {
      try {
        localStorage.setItem("demo-theme", next);
      } catch {
        // Theme changes still work for this visit when persistence is unavailable.
      }
      setTheme(next);
    },
  ];
}

/** The header's icon buttons: one hover and fill in either theme. */
export const ICON_BUTTON =
  "inline-flex size-10 shrink-0 items-center justify-center rounded-md text-muted-foreground transition-colors outline-none hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring [&_svg]:size-4";

export function ThemeToggle({ theme, onChange }: { theme: Theme; onChange: (theme: Theme) => void }) {
  const next = theme === "dark" ? "light" : "dark";
  return (
    <button type="button" className={ICON_BUTTON} onClick={() => onChange(next)} aria-label={`Switch to ${next} theme`}>
      {theme === "dark" ? <Sun aria-hidden="true" /> : <Moon aria-hidden="true" />}
    </button>
  );
}
