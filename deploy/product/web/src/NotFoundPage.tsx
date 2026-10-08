import { Button } from "@/components/ui/button";
import { CONTAINER, LINKS } from "./Site.js";

/** What Cloudflare serves, with a 404 status, for a path the site does not have. */
export function NotFoundPage() {
  return (
    <main id="top" className={`${CONTAINER} flex flex-1 flex-col justify-center py-24 lg:py-32`}>
      <p className="font-mono text-sm text-muted-foreground">404</p>
      <h1 className="mt-3 text-title-sm font-semibold sm:text-title">This page does not exist</h1>
      <p className="mt-4 max-w-md text-lead text-pretty text-body-foreground">
        The address may have a typo, or the page has moved. Start from the home page or the documentation.
      </p>
      <div className="mt-8 flex flex-col gap-3 sm:flex-row">
        <Button asChild size="lg"><a href="/">Go to the home page</a></Button>
        <Button asChild size="lg" variant="secondary"><a href={LINKS.docs}>Read the docs</a></Button>
      </div>
    </main>
  );
}
