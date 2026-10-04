import { schemas } from "./schemas.js";
import { isRecord } from "./json.js";
export interface Schema {
  $ref?: string;
  enum?: unknown[];
  type?: string | string[];
  required?: string[];
  properties?: Record<string, Schema>;
  additionalProperties?: boolean | Schema;
  items?: Schema;
  oneOf?: Schema[];
  anyOf?: Schema[];
  minimum?: number;
  maximum?: number;
}
export interface Contract {
  response: Schema;
  body?: Schema;
}
/** Open response enums and extra fields are retained for additive /v1 compatibility. */
export function valid(value: unknown, schema: Schema): boolean {
  if (schema.$ref) {
    const target = schemas[schema.$ref.split("/").at(-1) ?? ""];
    return target !== undefined && valid(value, target);
  }
  const alternatives = schema.oneOf ?? schema.anyOf;
  if (alternatives) return alternatives.some((candidate) => valid(value, candidate));
  const types = Array.isArray(schema.type) ? schema.type : [schema.type];
  if (value === null) return types.includes("null");
  if (types.includes("object")) {
    if (!isRecord(value) || schema.required?.some((key) => !Object.hasOwn(value, key)))
      return false;
    const known = Object.entries(schema.properties ?? {}).every(
      ([key, shape]) =>
        !Object.hasOwn(value, key) ||
        (value[key] === undefined && !schema.required?.includes(key)) ||
        valid(value[key], shape),
    );
    if (!known) return false;
    const extra = schema.additionalProperties;
    // Ignore unknown response fields; typed dictionaries still validate their values.
    return typeof extra !== "object" || Object.entries(value).every(([key, item]) => Object.hasOwn(schema.properties ?? {}, key) || valid(item, extra));
  }
  if (types.includes("array"))
    return (
      Array.isArray(value) &&
      (!schema.items || value.every((item) => valid(item, schema.items ?? {})))
    );
  if (types.includes("integer") || types.includes("number"))
    return (
      typeof value === "number" &&
      Number.isFinite(value) &&
      (!types.includes("integer") || Number.isSafeInteger(value)) &&
      (schema.minimum === undefined || value >= schema.minimum) &&
      (schema.maximum === undefined || value <= schema.maximum)
    );
  if (types.includes("string"))
    return typeof value === "string" && (!schema.enum || schema.enum.includes(value));
  if (types.includes("boolean")) return typeof value === "boolean";
  return true;
}
