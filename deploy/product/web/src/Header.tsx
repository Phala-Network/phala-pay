import { SiteHeader } from "./Site.js";
import { useTheme } from "./theme.js";

/** Shared component tree for server rendering and client hydration. */
export function Header() {
  const [theme, setTheme] = useTheme();
  return <SiteHeader theme={theme} onThemeChange={setTheme} />;
}
