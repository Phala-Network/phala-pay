import { resolve } from "node:path";

// The docs' Mermaid diagrams as the site shows them: static SVGs, light and dark, rendered by
// `npm run diagrams` (scripts/diagrams.ts) and committed under public/diagrams, like the generated
// OpenAPI document and SDK types; CI fails when they no longer match the markdown. The site's
// build (scripts/markdown.ts) only reads them.

/** Where the diagrams are committed; the site serves them at /diagrams. */
export const DIAGRAMS_DIR = resolve(import.meta.dirname, "../public/diagrams");

export const THEMES = ["light", "dark"] as const;
export type Theme = (typeof THEMES)[number];

/**
 * A diagram's file name, from its doc's path and its place among the doc's diagrams, from 1:
 * docs/overview.md's first, in the light theme, is docs-overview-1-light.svg.
 */
export function diagramName(file: string, index: number, theme: Theme): `${string}.svg` {
  return `${file.replace(/\.md$/, "").replaceAll("/", "-").toLowerCase()}-${index}-${theme}.svg`;
}

/** The fenced Mermaid blocks of a markdown source, in order, as the site's renderer meets them. */
export function mermaidBlocks(source: string): string[] {
  return [...source.matchAll(/^```mermaid[^\S\n]*\n([\s\S]*?)\n```[^\S\n]*$/gm)].map(([, body = ""]) => body);
}

const ENTITIES: Record<string, string> = { lt: "<", gt: ">", amp: "&", quot: '"' };

const clean = (label: string) => label
  .replace(/,?\s*<br\s*\/?>/g, ", ")
  // Each entity decoded once, in one pass: `&amp;lt;` is the text `&lt;`, not `<`.
  .replace(/&(lt|gt|amp|quot);/g, (_entity, name: string) => ENTITIES[name] ?? "")
  .replace(/\s+/g, " ")
  .trim();

/**
 * A diagram in words, for its alt text: its own accessible title and description where it
 * declares them (Mermaid's accTitle and accDescr); else what it draws, read from its source: a
 * flowchart's groups and the arrows between its nodes, or a sequence diagram's messages and notes
 * in order.
 */
export function describeDiagram(definition: string): string {
  const title = /^\s*accTitle:\s*(.+)$/m.exec(definition)?.[1]?.trim();
  const description = /^\s*accDescr:\s*(.+)$/m.exec(definition)?.[1]?.trim();
  if (title !== undefined || description !== undefined) return [title, description].filter(Boolean).join(". ");
  const lines = definition.split("\n").map((line) => line.trim()).filter((line) => line !== "" && !line.startsWith("%%"));
  const kind = lines[0] ?? "";
  if (/^(flowchart|graph)\b/.test(kind)) return describeFlowchart(lines.slice(1));
  if (/^sequenceDiagram\b/.test(kind)) return describeSequence(lines.slice(1));
  return `A ${kind.split(/\s/)[0] ?? "Mermaid"} diagram.`;
}

function describeFlowchart(lines: string[]): string {
  // A node by its label's first line in the arrows; with the rest, in its group's list.
  const labels = new Map<string, { name: string; detail: string }>();
  const groups: { label: string; members: string[] }[] = [];
  const open: { label: string; members: string[] }[] = [];
  const node = /(\w+)\s*(?:\(\[|\[\(|\(\(|\[|\(|\{)\s*"([^"]*)"/g;
  for (const line of lines) {
    const group = /^subgraph\s+(\w+)(?:\s*\[\s*"([^"]*)"\s*\])?/.exec(line);
    if (group !== null) {
      const entry = { label: clean(group[2] ?? group[1] ?? ""), members: [] as string[] };
      groups.push(entry);
      open.push(entry);
      continue;
    }
    if (line === "end") {
      open.pop();
      continue;
    }
    for (const [, id = "", label = ""] of line.matchAll(node)) {
      const [first = "", ...rest] = label.split(/<br\s*\/?>/);
      labels.set(id, { name: clean(first).replace(/,$/, ""), detail: clean(rest.join("<br/>")).replace(/^\((.*)\)$/, "$1") });
      open.at(-1)?.members.push(id);
    }
  }
  const name = (id: string) => labels.get(id)?.name ?? id;
  const full = (id: string) => { const label = labels.get(id); return label === undefined ? id : label.detail === "" ? label.name : `${label.name} (${label.detail})`; };
  const arrows: string[] = [];
  for (const line of lines) {
    const edge = /^(\w+)\b.*?\s(<?[-=.]+>)\s*(?:\|\s*"?([^"|]*)"?\s*\|)?\s*(\w+)\b/.exec(line.replace(/\[[^\]]*\]|\([^)]*\)/g, ""));
    if (edge === null) continue;
    const [, from = "", arrow = "", label, to = ""] = edge;
    const both = arrow.startsWith("<");
    arrows.push(`${name(from)} ${both ? "and" : "to"} ${name(to)}${label === undefined || label.trim() === "" ? "" : `: ${clean(label)}`}`);
  }
  const parts = groups.map(({ label, members }) => `${label}: ${members.map(full).join("; ")}`);
  return `Flowchart. ${[...parts, ...arrows].join(". ")}.`;
}

function describeSequence(lines: string[]): string {
  const names = new Map<string, string>();
  const steps: string[] = [];
  const conditions: string[] = [];
  for (const line of lines) {
    const participant = /^(?:participant|actor)\s+(\w+)(?:\s+as\s+(.+))?$/.exec(line);
    if (participant !== null) {
      names.set(participant[1] ?? "", clean(participant[2] ?? participant[1] ?? ""));
      continue;
    }
    const block = /^(opt|alt|loop|par|critical|break)\s+(.+)$/.exec(line);
    if (block !== null) {
      conditions.push(clean(block[2] ?? ""));
      continue;
    }
    if (line === "end") {
      conditions.pop();
      continue;
    }
    const prefix = conditions.length === 0 ? "" : `${conditions.at(-1) ?? ""}: `;
    const message = /^(\w+)\s*-{1,2}[>x)]{1,2}[+-]?\s*(\w+)\s*:\s*(.+)$/.exec(line);
    if (message !== null) {
      const name = (id: string) => names.get(id) ?? id;
      steps.push(`${prefix}${name(message[1] ?? "")} to ${name(message[2] ?? "")}: ${clean(message[3] ?? "")}`);
      continue;
    }
    const note = /^Note\s+(?:over|left of|right of)\s+[^:]+:\s*(.+)$/i.exec(line);
    if (note !== null) steps.push(`${prefix}Note: ${clean(note[1] ?? "")}`);
  }
  return `Sequence diagram. ${steps.map((step, index) => `${index + 1}. ${step}`).join(". ")}.`;
}
