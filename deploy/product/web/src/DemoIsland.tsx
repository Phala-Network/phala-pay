import { Component, lazy, Suspense, type ReactNode } from "react";
import { DemoLoading, DemoPlaceholder } from "./Site.js";
import { useTheme } from "./theme.js";
import { useHydrated } from "./islands.js";

const Demo = lazy(() => import("./Demo.js"));

class DemoBoundary extends Component<{ children: ReactNode }, { failed: boolean }> {
  state = { failed: false };
  static getDerivedStateFromError() { return { failed: true }; }
  render() {
    return this.state.failed
      ? <p role="alert" className="text-sm text-muted-foreground">The demo could not load. Reload the page to try again.</p>
      : this.props.children;
  }
}

/** The demo is the only interactive island in the home page's main content. */
export function DemoIsland() {
  const [theme] = useTheme();
  const isClient = useHydrated();
  if (!isClient) return <DemoPlaceholder />;
  return (
    <DemoBoundary>
      <Suspense fallback={<DemoLoading />}><Demo theme={theme} /></Suspense>
    </DemoBoundary>
  );
}
