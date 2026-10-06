# Hero snippets

The integration code the home page's hero shows, imported with its syntax tokens
(`?highlight`, scripts/highlight.ts) and shown as written. They are source files so that
`pnpm run typecheck` compiles them against the SDK packages (`@phala/pay-server`, `@phala/pay`,
`@phala/pay-react`): a snippet that stops compiling fails the check. Nothing imports them as code;
they are never bundled. Each is at most 14 lines, the hero's code window, and its lines stay within
66 characters, the window's width at 1280px; narrower, the window scrolls sideways.
