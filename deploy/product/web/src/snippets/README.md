# Hero snippets

The integration code the home page's hero shows, imported as text (`?raw`) and shown as written.
They are source files so that `pnpm run typecheck` compiles them against the SDK packages
(`@phala/pay-server`, `@phala/pay`, `@phala/pay-react`): a snippet that stops compiling fails the
check. Nothing imports them as code; they are never bundled. Lines stay within 58 characters, the
width of the hero's code column at 1280px.
