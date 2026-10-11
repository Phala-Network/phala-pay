import { Component, Suspense, type ComponentType, type ReactNode } from "react";
import { DemoLoading } from "./Site.js";

class DemoBoundary extends Component<{ children: ReactNode }, { failed: boolean }> {
  state = { failed: false };
  static getDerivedStateFromError() { return { failed: true }; }
  render() {
    return this.state.failed
      ? <p role="alert" className="text-sm text-muted-foreground">The demo could not load. Reload the page to try again.</p>
      : this.props.children;
  }
}

/**
 * The demo, the only interactive island in the home page's main content. The prerender passes the
 * demo itself (src/entry-server.tsx), so the page arrives with both panels in their loading state;
 * the client passes it lazily (src/main.tsx), its code a chunk of its own, and React keeps the
 * prerendered panels in place until it has loaded, then hydrates them. Nothing crosses the boundary
 * as a prop: an update there before the demo hydrated would make React discard the prerendered
 * panels for the fallback.
 */
export function DemoIsland({ demo: Demo }: { demo: ComponentType }) {
  return (
    <DemoBoundary>
      <Suspense fallback={<DemoLoading />}><Demo /></Suspense>
    </DemoBoundary>
  );
}
