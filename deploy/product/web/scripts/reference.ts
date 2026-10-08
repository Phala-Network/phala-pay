import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { tokenize, type Token } from "./highlight.ts";
import { renderFragment as renderMarkdown, renderSection, sectionSlug } from "./markdown.ts";

/**
 * The API reference's model, built at build time from the committed OpenAPI document
 * (crates/topup/openapi.json): every description rendered from markdown, every `$ref` named, and
 * each operation's request and response examples highlighted. src/ReferencePage.tsx renders it as
 * static HTML. Anchors follow the ones Redoc gave the previous reference (`section/Errors/<code>`,
 * `tag/<tag>/operation/<operationId>`), which error objects' `doc_url` and existing links use.
 */
const SPEC_FILE = "crates/topup/openapi.json";
const SPEC = resolve(import.meta.dirname, "../../../..", SPEC_FILE);
const renderFragment = (source: string) => renderMarkdown(source, SPEC_FILE);
const METHODS = ["get", "post", "put", "patch", "delete"] as const;

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
interface Schema {
  $ref?: string; type?: string | string[]; format?: string; description?: string; enum?: Json[];
  items?: Schema; properties?: Record<string, Schema>; required?: string[]; oneOf?: Schema[]; anyOf?: Schema[];
  additionalProperties?: boolean | Schema; example?: Json; minimum?: number; maximum?: number; nullable?: boolean;
}
interface Parameter { name: string; in: string; required?: boolean; description?: string; schema?: Schema }
interface Response { description: string; content?: Record<string, { schema?: Schema }> }
interface Operation {
  operationId: string; summary?: string; description?: string; tags?: string[]; parameters?: Parameter[];
  requestBody?: { required?: boolean; content?: Record<string, { schema?: Schema }> }; responses: Record<string, Response>;
}
interface Spec {
  info: { title: string; version: string; description: string };
  servers: { url: string; description?: string }[];
  tags: { name: string; description?: string }[];
  paths: Record<string, Partial<Record<(typeof METHODS)[number], Operation>>>;
  components: { schemas: Record<string, Schema>; securitySchemes: Record<string, { description?: string }> };
}

/** A schema's type, as the reference writes it: a named object links to its definition. */
export type TypeNode =
  | { kind: "ref"; name: string }
  | { kind: "array"; items: TypeNode }
  | { kind: "map"; values: TypeNode }
  | { kind: "union"; variants: TypeNode[] }
  | { kind: "scalar"; type: string; format?: string; values?: string[]; nullable: boolean };
export interface Field { name: string; required: boolean; type: TypeNode; html: string; fields: Field[] }
export interface ResponseModel { status: string; html: string; type?: TypeNode }
export interface OperationModel {
  id: string; anchor: string; method: string; path: string; summary: string; html: string;
  parameters: { in: string; fields: Field[] }[]; body?: { type: TypeNode; fields: Field[] };
  responses: ResponseModel[]; request: Token[][];
}
export interface ReferenceModel {
  title: string; version: string; servers: { url: string; description: string }[];
  /** The introduction's opening, before its first section. */
  lead: string;
  intro: { id: string; title: string; html: string; children: { id: string; title: string }[] }[];
  tags: { name: string; title: string; anchor: string; html: string; operations: OperationModel[] }[];
  schemas: { name: string; anchor: string; html: string; type: TypeNode; fields: Field[]; example?: Token[][] }[];
}

const refName = (ref: string) => ref.split("/").at(-1) ?? ref;
/** `deposit_addresses` → `Deposit addresses`. */
const humanize = (name: string) => name.replaceAll("_", " ").replace(/^./, (letter) => letter.toUpperCase());

function typeOf(schema: Schema | undefined): TypeNode {
  if (schema === undefined) return { kind: "scalar", type: "any", nullable: false };
  if (schema.$ref !== undefined) return { kind: "ref", name: refName(schema.$ref) };
  const variants = schema.oneOf ?? schema.anyOf;
  if (variants !== undefined) return { kind: "union", variants: variants.map(typeOf) };
  const types = Array.isArray(schema.type) ? schema.type : schema.type === undefined ? [] : [schema.type];
  const nullable = types.includes("null") || schema.nullable === true;
  const type = types.find((each) => each !== "null") ?? "any";
  if (type === "array") return { kind: "array", items: typeOf(schema.items) };
  if (type === "object" && schema.properties === undefined && typeof schema.additionalProperties === "object") {
    return { kind: "map", values: typeOf(schema.additionalProperties) };
  }
  const node: TypeNode = { kind: "scalar", type, nullable };
  if (schema.format !== undefined) node.format = schema.format;
  if (schema.enum !== undefined) node.values = schema.enum.map((value) => JSON.stringify(value));
  return node;
}

async function fieldsOf(schema: Schema | undefined, depth = 0): Promise<Field[]> {
  if (schema?.properties === undefined) return [];
  const required = new Set(schema.required ?? []);
  return Promise.all(Object.entries(schema.properties).map(async ([name, property]) => ({
    name,
    required: required.has(name),
    type: typeOf(property),
    html: property.description === undefined ? "" : await renderFragment(property.description),
    // An inline object's own fields, one level down; named objects link to their definition.
    fields: depth < 2 && property.$ref === undefined ? await fieldsOf(property.items ?? property, depth + 1) : [],
  })));
}

function jsonOf(schema: Schema | undefined, schemas: Record<string, Schema>): Json | undefined {
  if (schema === undefined) return undefined;
  if (schema.example !== undefined) return schema.example;
  if (schema.$ref !== undefined) return schemas[refName(schema.$ref)]?.example;
  if (schema.items !== undefined) {
    const item = jsonOf(schema.items, schemas);
    return item === undefined ? undefined : [item];
  }
  return undefined;
}

function curl(method: string, url: string, body: Json | undefined): string {
  const lines = [`curl ${method === "get" ? "" : `-X ${method.toUpperCase()} `}${url}`, `  -H "Authorization: Bearer $PHALA_PAY_API_KEY"`];
  if (body !== undefined) {
    lines.push(`  -H "Content-Type: application/json"`, `  -d '${JSON.stringify(body, null, 2).replaceAll("\n", "\n  ")}'`);
  }
  return lines.join(" \\\n");
}

/** The introduction: its opening, then a section per H1 (Authentication, …, Errors). */
async function introduction(description: string): Promise<Pick<ReferenceModel, "lead" | "intro">> {
  const [opening = "", ...rest] = description.split(/^# /m);
  return {
    lead: await renderFragment(opening.trim()),
    intro: await Promise.all(rest.map(async (part) => {
      const [title = "", ...body] = part.split("\n");
      const id = `section/${sectionSlug(title)}`;
      return { id, title: title.trim(), ...(await renderSection(body.join("\n"), SPEC_FILE, id)) };
    })),
  };
}

export async function buildReference(): Promise<ReferenceModel> {
  const spec = JSON.parse(await readFile(SPEC, "utf8")) as Spec;
  const schemas = spec.components.schemas;
  const server = spec.servers[0]?.url ?? "";
  const operations = Object.entries(spec.paths).flatMap(([path, item]) =>
    METHODS.flatMap((method) => (item[method] === undefined ? [] : [{ method, path, operation: item[method] }])));
  // The document's tags in its order, then any an operation uses without declaring it (as Redoc
  // lists them), so that every operation has its section.
  const declared = spec.tags.map(({ name }) => name);
  const undeclared = [...new Set(operations.flatMap(({ operation }) => operation.tags?.slice(0, 1) ?? []))]
    .filter((name) => !declared.includes(name))
    .map((name) => ({ name, description: undefined }));
  const tags = await Promise.all([...spec.tags, ...undeclared].map(async ({ name, description }) => ({
    name,
    title: humanize(name),
    anchor: `tag/${name}`,
    html: description === undefined ? "" : await renderFragment(description),
    operations: await Promise.all(operations.filter(({ operation }) => operation.tags?.[0] === name).map(async ({ method, path, operation }) => {
      const json = operation.requestBody?.content?.["application/json"]?.schema;
      const parameters = ["path", "query", "header"].map((place) => ({
        in: place,
        fields: (operation.parameters ?? []).filter((parameter) => parameter.in === place),
      })).filter(({ fields }) => fields.length > 0);
      const model: OperationModel = {
        id: operation.operationId,
        anchor: `tag/${name}/operation/${operation.operationId}`,
        method,
        path,
        // The operation's name (`create_quote` → "Create quote"), then its summary and description.
        summary: humanize(operation.operationId),
        html: await renderFragment([operation.summary, operation.description].filter((part) => part !== undefined).join("\n\n")),
        parameters: await Promise.all(parameters.map(async ({ in: place, fields }) => ({
          in: place,
          fields: await Promise.all(fields.map(async (parameter) => ({
            name: parameter.name,
            required: parameter.required === true,
            type: typeOf(parameter.schema),
            html: parameter.description === undefined ? "" : await renderFragment(parameter.description),
            fields: [],
          }))),
        }))),
        responses: await Promise.all(Object.entries(operation.responses).map(async ([status, response]) => {
          const type = response.content?.["application/json"]?.schema;
          return { status, html: await renderFragment(response.description), ...(type === undefined ? {} : { type: typeOf(type) }) };
        })),
        request: await tokenize(curl(method, `${server}${path}`, jsonOf(json, schemas)), "sh"),
      };
      if (json !== undefined) model.body = { type: typeOf(json), fields: await fieldsOf(json.$ref === undefined ? json : schemas[refName(json.$ref)]) };
      return model;
    })),
  })));
  // Every operation of the document is on the page: one without a tag would have no section.
  const rendered = tags.reduce((count, { operations: listed }) => count + listed.length, 0);
  if (rendered !== operations.length) {
    throw new Error(`The API reference renders ${rendered} of the document's ${operations.length} operations`);
  }
  return {
    title: spec.info.title,
    version: spec.info.version,
    servers: spec.servers.map(({ url, description }) => ({ url, description: description ?? "" })),
    ...(await introduction(spec.info.description)),
    tags,
    schemas: await Promise.all(Object.entries(schemas).sort(([left], [right]) => left.localeCompare(right)).map(async ([name, schema]) => {
      const model: ReferenceModel["schemas"][number] = {
        name,
        anchor: `schema/${name}`,
        html: schema.description === undefined ? "" : await renderFragment(schema.description),
        type: typeOf(schema),
        fields: await fieldsOf(schema),
      };
      if (schema.example !== undefined) model.example = await tokenize(JSON.stringify(schema.example, null, 2), "json");
      return model;
    })),
  };
}
