# Hero snippets

The integration code the home page's hero shows, imported with its syntax tokens
(`?highlight`, scripts/highlight.ts) and shown as written. They are source files so that
`pnpm run typecheck` compiles them against the SDK packages (`@phala/pay-server`, `@phala/pay`,
`@phala/pay-react`): a snippet that stops compiling fails the check. Nothing imports them as code;
they are never bundled. Their lines stay within 45 characters, so that they fit the hero's code
window at every width, a 390px phone's too (an e2e test holds them inside it). The window is as
tall as the longer snippet.
