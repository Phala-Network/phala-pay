import { Component, lazy, Suspense, type ReactNode } from "react";
import { ClosingCta, CompareTeaser, DemoPlaceholder, DemoSection, Faq, Hero, HowItWorks, Properties, SiteFooter, SiteHeader } from "./Site.js";
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

export function App() {
  const [theme, setTheme] = useTheme();
  return (
    <div className="flex min-h-svh flex-col">
      <SiteHeader theme={theme} onThemeChange={setTheme} />
      <main id="top" className="flex-1">
        <Hero />
        <HowItWorks />
        <DemoSection>
          <DemoBoundary>
            <Suspense fallback={<DemoPlaceholder />}><Demo theme={theme} /></Suspense>
          </DemoBoundary>
        </DemoSection>
        <Properties />
        <CompareTeaser />
        <Faq />
        <ClosingCta />
      </main>
      <SiteFooter />
    </div>
  );
}
