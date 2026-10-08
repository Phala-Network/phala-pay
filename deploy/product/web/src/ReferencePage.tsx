import type { ReactNode } from "react";
import type { Field, OperationModel, ReferenceModel, TypeNode } from "../scripts/reference.ts";
import { CodeBody, CodeWindow, HighlightedLines } from "@/components/code";
import { cn } from "@/lib/utils";
import { CONTAINER } from "./Site.js";
import { DocsMobileNav, PROSE } from "./DocsPage.js";

/** A method as a small mono label, its colour from the method's family (read or write). */
function Method({ method }: { method: string }) {
  return (
    <span className={cn(
      "inline-flex h-5 min-w-12 items-center justify-center rounded-sm px-1.5 font-mono text-xs font-semibold uppercase",
      method === "get" ? "bg-muted text-foreground" : method === "delete" ? "bg-destructive-muted text-foreground" : "bg-foreground text-background",
    )}>
      {method}
    </span>
  );
}

/**
 * A schema's name, free to wrap where its words meet (`SubmitDepositAddress` / `TransactionRequest`
 * on a phone) and nowhere else.
 */
function SchemaName({ name }: { name: string }) {
  return name.split(/(?=[A-Z])/).map((part, index) => <span key={index}>{index > 0 && <wbr />}{part}</span>);
}

/** A type as the reference writes it: named objects link to their definition. */
function TypeLabel({ type }: { type: TypeNode }): ReactNode {
  switch (type.kind) {
    case "ref":
      return <a href={`#schema/${type.name}`} className="text-foreground underline decoration-foreground/30 underline-offset-2 hover:decoration-foreground"><SchemaName name={type.name} /></a>;
    case "array":
      return <>array of <TypeLabel type={type.items} /></>;
    case "map":
      return <>map of <TypeLabel type={type.values} /></>;
    case "union":
      return type.variants.map((variant, index) => <span key={index}>{index > 0 && " or "}<TypeLabel type={variant} /></span>);
    case "scalar":
      return (
        <>
          {type.values === undefined ? type.type : type.values.join(" | ")}
          {type.format !== undefined && <span className="text-muted-foreground"> ({type.format})</span>}
          {type.nullable && " or null"}
        </>
      );
  }
}

/** Fields: name, type, and whether required on one line, the description under them. */
function Fields({ fields, nested = false }: { fields: Field[]; nested?: boolean }) {
  return (
    <dl className={cn("divide-y border-y", nested && "mt-3 border-b-0 pl-4")}>
      {fields.map((field) => (
        <div key={field.name} className="py-3">
          <dt className="flex flex-wrap items-baseline gap-x-2 gap-y-1">
            <code className="font-mono text-mono font-semibold text-foreground">{field.name}</code>
            <span className="font-mono text-xs text-muted-foreground"><TypeLabel type={field.type} /></span>
            {field.required && <span className="text-xs font-medium text-foreground">required</span>}
          </dt>
          <dd>
            {field.html !== "" && <div className={cn(PROSE, "mt-1 text-sm")} dangerouslySetInnerHTML={{ __html: field.html }} />}
            {field.fields.length > 0 && (
              <details className="group mt-2">
                <summary className="inline-flex min-h-8 cursor-pointer items-center rounded-md border px-2.5 text-xs font-medium text-muted-foreground hover:text-foreground">
                  {field.fields.length} child field{field.fields.length === 1 ? "" : "s"}
                </summary>
                <Fields fields={field.fields} nested />
              </details>
            )}
          </dd>
        </div>
      ))}
    </dl>
  );
}

function Example({ title, label, lines }: { title: string; label: string; lines: Parameters<typeof HighlightedLines>[0]["lines"] }) {
  return (
    <CodeWindow header={<span className="text-sm text-code-muted">{title}</span>}>
      <CodeBody label={label}>
        <HighlightedLines lines={lines} />
      </CodeBody>
    </CodeWindow>
  );
}

function OperationSection({ operation }: { operation: OperationModel }) {
  const success = operation.responses.find(({ status }) => status.startsWith("2"));
  return (
    <section id={operation.anchor} aria-labelledby={`${operation.anchor}-title`} className="scroll-mt-20 border-t py-12">
      <div className="grid gap-8 xl:grid-cols-[minmax(0,1fr)_minmax(0,26rem)] xl:gap-12">
        <div className="min-w-0">
          <h3 id={`${operation.anchor}-title`} className="text-heading font-semibold">{operation.summary}</h3>
          <p className="mt-2 flex flex-wrap items-center gap-2 font-mono text-mono">
            <Method method={operation.method} />
            <span className="break-all">{operation.path}</span>
          </p>
          {operation.html !== "" && <div className={cn(PROSE, "mt-4")} dangerouslySetInnerHTML={{ __html: operation.html }} />}
          {operation.parameters.map(({ in: place, fields }) => (
            <div key={place} className="mt-8">
              <h4 className="mb-2 text-sm font-semibold">{place === "path" ? "Path parameters" : place === "query" ? "Query parameters" : "Headers"}</h4>
              <Fields fields={fields} />
            </div>
          ))}
          {operation.body !== undefined && (
            <div className="mt-8">
              <h4 className="mb-2 text-sm font-semibold">Request body <span className="font-normal text-muted-foreground">· <TypeLabel type={operation.body.type} /></span></h4>
              <Fields fields={operation.body.fields} />
            </div>
          )}
          <div className="mt-8">
            <h4 className="mb-2 text-sm font-semibold">Responses</h4>
            <dl className="divide-y border-y">
              {operation.responses.map(({ status, html, type }) => (
                <div key={status} className="grid grid-cols-[3.5rem_minmax(0,1fr)] gap-3 py-2.5 text-sm">
                  <dt className={cn("font-mono font-semibold", status.startsWith("2") ? "text-success" : "text-foreground")}>{status}</dt>
                  <dd className="min-w-0">
                    <div className={cn(PROSE, "text-sm [&_p]:m-0")} dangerouslySetInnerHTML={{ __html: html }} />
                    {type !== undefined && status.startsWith("2") && (
                      <p className="mt-1 font-mono text-xs text-muted-foreground">Returns <TypeLabel type={type} /></p>
                    )}
                  </dd>
                </div>
              ))}
            </dl>
          </div>
        </div>
        {/* The request; the response is the object it returns, whose example is under Objects (once
            for every operation that returns it). In the page's flow: only the navigation scrolls on
            its own. */}
        <div className="flex min-w-0 flex-col gap-3">
          <Example title="Request" label={`${operation.summary}: request`} lines={operation.request} />
          {success?.type !== undefined && (
            <p className="text-sm text-muted-foreground">
              Response {success.status}: <span className="font-mono text-xs"><TypeLabel type={success.type} /></span>
            </p>
          )}
        </div>
      </div>
    </section>
  );
}

/** The reference's navigation: the introduction's sections, then each tag with its operations. */
function ReferenceNav({ model }: { model: ReferenceModel }) {
  return (
    <nav aria-label="API reference" className="text-sm">
      <p className="px-2 pb-2 text-xs font-medium text-muted-foreground">Introduction</p>
      <ul>
        {model.intro.map(({ id, title }) => (
          <li key={id}><a href={`#${id}`} className="flex min-h-8 items-center rounded-md px-2 text-body-foreground hover:bg-muted hover:text-foreground">{title}</a></li>
        ))}
      </ul>
      {model.tags.map(({ name, title, anchor, operations }) => (
        <div key={name} className="mt-5">
          <a href={`#${anchor}`} className="block px-2 pb-1 text-xs font-medium text-muted-foreground hover:text-foreground">{title}</a>
          <ul>
            {operations.map((operation) => (
              <li key={operation.id}>
                <a href={`#${operation.anchor}`} className="flex min-h-11 items-baseline gap-2 rounded-md px-2 py-1.5 text-body-foreground hover:bg-muted hover:text-foreground lg:min-h-8">
                  <span className="w-12 shrink-0 font-mono text-xs font-semibold text-muted-foreground uppercase">{operation.method}</span>
                  <span className="min-w-0">{operation.summary}</span>
                </a>
              </li>
            ))}
          </ul>
        </div>
      ))}
      <a href="#objects" className="mt-5 block px-2 text-xs font-medium text-muted-foreground hover:text-foreground">Objects</a>
    </nav>
  );
}

export function ReferencePage({ model }: { model: ReferenceModel }) {
  return (
    <div className={`${CONTAINER} flex-1 lg:grid lg:grid-cols-[15rem_minmax(0,1fr)] lg:gap-10`}>
      {/* The one part of the site that scrolls on its own: the reference's navigation. */}
      <aside className="hidden lg:block">
        <div className="sticky top-16 max-h-[calc(100svh-4rem)] overflow-y-auto py-10 pr-2">
          <ReferenceNav model={model} />
        </div>
      </aside>
      <main id="top" className="min-w-0 pt-10 pb-24">
        <DocsMobileNav label="API reference menu"><ReferenceNav model={model} /></DocsMobileNav>
        <p className="text-sm font-medium text-muted-foreground">API reference · v{model.version}</p>
        <h1 className="mt-2 text-title-sm font-semibold sm:text-title">{model.title}</h1>
        <div className={cn(PROSE, "mt-4 max-w-3xl")} dangerouslySetInnerHTML={{ __html: model.lead }} />
        <dl className="mt-6 grid gap-2 text-sm sm:grid-cols-2">
          {model.servers.map(({ url, description }) => (
            <div key={url} className="rounded-lg border px-4 py-3">
              <dt className="text-muted-foreground">{description}</dt>
              <dd className="mt-1 font-mono text-mono break-all">{url}</dd>
            </div>
          ))}
        </dl>
        {model.intro.map(({ id, title, html }) => (
          <section key={id} id={id} aria-labelledby={`${id}-title`} className="scroll-mt-20 border-t py-10 first-of-type:mt-10">
            <h2 id={`${id}-title`} className="text-title-sm font-semibold">{title}</h2>
            <div className={cn(PROSE, "mt-4 max-w-3xl")} dangerouslySetInnerHTML={{ __html: html }} />
          </section>
        ))}
        {model.tags.map(({ name, title, anchor, html, operations }) => (
          <section key={name} id={anchor} aria-labelledby={`${anchor}-title`} className="scroll-mt-20 pt-16">
            <h2 id={`${anchor}-title`} className="text-title-sm font-semibold">{title}</h2>
            {html !== "" && <div className={cn(PROSE, "mt-3 max-w-3xl")} dangerouslySetInnerHTML={{ __html: html }} />}
            <div className="mt-6">
              {operations.map((operation) => <OperationSection key={operation.id} operation={operation} />)}
            </div>
          </section>
        ))}
        <section id="objects" aria-labelledby="objects-title" className="scroll-mt-20 pt-16">
          <h2 id="objects-title" className="text-title-sm font-semibold">Objects</h2>
          {model.schemas.map(({ name, anchor, html, fields, example }) => (
            <section key={name} id={anchor} aria-labelledby={`${anchor}-title`} className="scroll-mt-20 border-t py-10">
              <div className="grid gap-8 xl:grid-cols-[minmax(0,1fr)_minmax(0,26rem)] xl:gap-12">
                <div className="min-w-0">
                  <h3 id={`${anchor}-title`} className="font-mono text-base font-semibold"><SchemaName name={name} /></h3>
                  {html !== "" && <div className={cn(PROSE, "mt-2")} dangerouslySetInnerHTML={{ __html: html }} />}
                  {fields.length > 0 && <div className="mt-4"><Fields fields={fields} /></div>}
                </div>
                {example !== undefined && <div className="min-w-0"><Example title="Example" label={`${name} example`} lines={example} /></div>}
              </div>
            </section>
          ))}
        </section>
      </main>
    </div>
  );
}
