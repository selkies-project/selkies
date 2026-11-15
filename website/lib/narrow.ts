/**
 * Narrowing in place of type assertions, for the site's tooling: a parsed
 * value is checked where it arrives and handed on typed, or not at all.
 *
 * The site directory carries its own, because every release's pages are built
 * with this tree's site directory over that release's source, where the web
 * client's helpers (web/selkies-web-core/lib/narrow.ts) may not exist.
 * @module
 */

/** A value read field by field, each field still to be checked. */
export type Fields = Record<string, unknown>;

/**
 * Whether a value is an object to read fields from.
 * @param value Anything.
 * @returns True for a non-null object, arrays included.
 */
export function isRecord(value: unknown): value is Fields {
  return typeof value === 'object' && value !== null;
}

/**
 * A value's fields, each still to be checked.
 * @param value Anything.
 * @returns `value` when it is an object, else an object with no fields.
 */
export function fields(value: unknown): Fields {
  return isRecord(value) ? value : {};
}

/**
 * A value when it is a string.
 * @param value Anything.
 * @returns `value`, or undefined when it is not a string.
 */
export function stringOr(value: unknown): string | undefined {
  return typeof value === 'string' ? value : undefined;
}
