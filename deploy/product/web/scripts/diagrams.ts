// Renders every Mermaid diagram of the docs the site shows (src/content/docs.ts) to static SVGs,
// light and dark, into public/diagrams (scripts/diagram-files.ts), and removes those whose diagram
// is gone. Run it as `npm run diagrams` (scripts/diagrams.sh), in Playwright's image: its Chromium
// lays the diagrams out, so every machine and CI write the same bytes. Each SVG embeds the Geist
// it is set in, so an <img> shows it as drawn; ids are deterministic.
import { mkdtemp, readdir, readFile, rm, writeFile, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { run } from "@mermaid-js/mermaid-cli";
import { chromium } from "@playwright/test";
import { DOCS } from "../src/content/docs.ts";
import { DIAGRAMS_DIR, THEMES, diagramName, mermaidBlocks, type Theme } from "./diagram-files.ts";

const REPO_ROOT = resolve(import.meta.dirname, "../../../..");

interface Palette { node: string; border: string; text: string; line: string; group: string; groupBorder: string; note: string; ground: string }

/** The site's palette (src/index.css), on a transparent ground, in each theme. */
const PALETTE: Record<Theme, Palette> = {
  light: { node: "#ffffff", border: "#d4d4d4", text: "#1a1a1a", line: "#737373", group: "#fafafa", groupBorder: "#e5e5e5", note: "#f5f5f5", ground: "#ffffff" },
  dark: { node: "#232323", border: "#3d3d3d", text: "#f2f2f2", line: "#a3a3a3", group: "#1c1c1c", groupBorder: "#2f2f2f", note: "#2a2a2a", ground: "#141414" },
};

function config(theme: Theme, seed: string) {
  const colour = PALETTE[theme];
  return {
    theme: "base" as const,
    deterministicIds: true,
    deterministicIDSeed: seed,
    // Some shapes' outlines are drawn by rough.js, which is otherwise seeded at random.
    handDrawnSeed: 1,
    htmlLabels: false,
    flowchart: { htmlLabels: false, curve: "basis" as const },
    themeVariables: {
      fontFamily: "Geist Variable, sans-serif",
      fontSize: "14px",
      background: "transparent",
      primaryColor: colour.node,
      primaryBorderColor: colour.border,
      primaryTextColor: colour.text,
      secondaryColor: colour.group,
      tertiaryColor: colour.group,
      lineColor: colour.line,
      textColor: colour.text,
      mainBkg: colour.node,
      nodeBorder: colour.border,
      clusterBkg: colour.group,
      clusterBorder: colour.groupBorder,
      titleColor: colour.text,
      edgeLabelBackground: colour.ground,
      actorBkg: colour.node,
      actorBorder: colour.border,
      actorTextColor: colour.text,
      actorLineColor: colour.border,
      signalColor: colour.line,
      signalTextColor: colour.text,
      labelBoxBkgColor: colour.node,
      labelBoxBorderColor: colour.border,
      labelTextColor: colour.text,
      loopTextColor: colour.text,
      noteBkgColor: colour.note,
      noteBorderColor: colour.border,
      noteTextColor: colour.text,
      activationBkgColor: colour.group,
      activationBorderColor: colour.border,
      sequenceNumberColor: colour.ground,
    },
  };
}

const work = await mkdtemp(join(tmpdir(), "diagrams-"));
const written = new Set<string>();
try {
  await mkdir(DIAGRAMS_DIR, { recursive: true });
  for (const { file } of DOCS) {
    const blocks = mermaidBlocks(await readFile(resolve(REPO_ROOT, file), "utf8"));
    for (const [at, definition] of blocks.entries()) {
      for (const theme of THEMES) {
        const name = diagramName(file, at + 1, theme);
        const input = join(work, `${name}.mmd`);
        await writeFile(input, definition);
        const output: `${string}.svg` = `${DIAGRAMS_DIR}/${name}`;
        await run(input, output, {
          quiet: true,
          // Playwright's Chromium, launched as Playwright launches it in a container (unsandboxed:
          // its chromiumSandbox default); it only lays out the docs' own diagrams.
          puppeteerConfig: { executablePath: chromium.executablePath(), args: ["--no-sandbox"] },
          parseMMDOptions: {
            backgroundColor: "transparent",
            mermaidConfig: config(theme, name),
            customFontCSS: [{ cssUrl: new URL(import.meta.resolve("@fontsource-variable/geist/wght.css")) }],
            svgId: name.replace(/\.svg$/, ""),
          },
        });
        written.add(name);
        console.log(`${file} #${at + 1} ${theme}: public/diagrams/${name}`);
      }
    }
  }
  for (const name of await readdir(DIAGRAMS_DIR)) {
    if (name.endsWith(".svg") && !written.has(name)) {
      await rm(join(DIAGRAMS_DIR, name));
      console.log(`removed public/diagrams/${name}: its diagram is gone`);
    }
  }
} finally {
  await rm(work, { recursive: true, force: true });
}
