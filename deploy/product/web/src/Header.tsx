import { SiteHeader, type Section } from "./Site.js";
import { useTheme } from "./theme.js";

/**
 * Shared component tree for server rendering and client hydration: the page's section comes from
 * the prerender, and on the client from what it wrote (the island's data-current), so both match.
 */
export function Header({ current }: { current: Section | null }) {
  const [theme, setTheme] = useTheme();
  return <SiteHeader theme={theme} onThemeChange={setTheme} current={current} />;
}
