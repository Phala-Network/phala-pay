import { Component, lazy, Suspense, type ReactNode } from "react";
import { DemoPlaceholder } from "./Site.js";
import { useTheme } from "./theme.js";

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
  return (
    <DemoBoundary>
      <Suspense fallback={<DemoPlaceholder />}><Demo theme={theme} /></Suspense>
    </DemoBoundary>
  );
}
