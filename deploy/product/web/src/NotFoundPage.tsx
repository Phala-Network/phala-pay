import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { CONTAINER, Eyebrow, H2, LEAD, LINKS } from "./Site.js";

/** What Cloudflare serves, with a 404 status, for a path the site does not have. */
export function NotFoundPage() {
  return (
    <main id="top" className={`${CONTAINER} flex flex-1 flex-col justify-center py-28 lg:py-40`}>
      <Eyebrow>404 · Not found</Eyebrow>
      <h1 className={cn(H2, "mt-4")}>This page does not exist</h1>
      <p className={cn(LEAD, "mt-6 max-w-lg")}>
        The address may have a typo, or the page has moved. Start from the home page or the documentation.
      </p>
      <div className="mt-10 flex flex-col gap-3 sm:flex-row">
        <Button asChild size="lg"><a href="/">Go to the home page</a></Button>
        <Button asChild size="lg" variant="secondary"><a href={LINKS.docs}>Read the docs</a></Button>
      </div>
    </main>
  );
}
