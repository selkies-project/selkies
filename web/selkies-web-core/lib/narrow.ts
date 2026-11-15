/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Narrowing in place of type assertions. A value whose type the code cannot
 * know (a parsed message, a stored string, a caught error, a DOM lookup) is
 * checked at the point it arrives and handed on typed, or not at all.
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

/** What a field holds, as `typeof` names it, with `null` apart. */
export type FieldKind = 'string' | 'number' | 'boolean' | 'object' | 'null' | 'undefined';

/**
 * Whether a value is an object whose named fields each hold one of the kinds
 * listed for it, for a guard that states a wire shape once.
 * @param value Anything.
 * @param kinds Each field's kind, or the kinds it may be (`['number', 'null']`).
 * @returns True where every named field matches.
 */
export function hasFields(value: unknown, kinds: Readonly<Record<string, FieldKind | readonly FieldKind[]>>): value is Fields {
    if (!isRecord(value)) return false;
    return Object.entries(kinds).every(([name, kind]): boolean => {
        const field = value[name];
        const got = field === null ? 'null' : typeof field;
        return (typeof kind === 'string' ? [kind] : kind).some((k: FieldKind): boolean => k === got);
    });
}

/**
 * A value as an object to read fields from, or an empty one.
 * @param value Anything.
 * @returns `value` when it is an object, else a fresh empty object.
 */
export function fields(value: unknown): Fields {
    return isRecord(value) ? value : {};
}

/**
 * A value when it is one of a fixed set.
 * @param value Anything.
 * @param options The values allowed.
 * @returns `value` narrowed to the set, or undefined when it is not in it.
 */
export function oneOf<T>(value: unknown, options: readonly T[]): T | undefined {
    return options.find((option) => option === value);
}

/**
 * A value when it is a string.
 * @param value Anything.
 * @returns `value`, or undefined when it is not a string.
 */
export function stringOr(value: unknown): string | undefined {
    return typeof value === 'string' ? value : undefined;
}

/**
 * A value when it is a finite number.
 * @param value Anything.
 * @returns `value`, or undefined when it is not a finite number.
 */
export function numberOr(value: unknown): number | undefined {
    return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

/**
 * The message a caught value carries, where it carries one.
 * @param err The value a `catch` received.
 * @returns Its `message` when that is a string (an Error, a DOMException, or
 *     an object shaped like one), else undefined.
 */
export function messageOf(err: unknown): string | undefined {
    return isRecord(err) ? stringOr(err.message) : undefined;
}

/**
 * What a caught value says, whatever was thrown.
 * @param err The value a `catch` received.
 * @returns Its message, or the value as a string where it carries none.
 */
export function errorMessage(err: unknown): string {
    return messageOf(err) ?? String(err);
}

/**
 * A caught value as an Error, for a callback that takes one.
 * @param err The value a `catch` received.
 * @returns The value where it is an Error (a DOMException is one), else an
 *     Error carrying what it says.
 */
export function asError(err: unknown): Error {
    return err instanceof Error ? err : new Error(errorMessage(err));
}

/**
 * A caught value's name where it is an error (a DOMException's included).
 * @param err The value a `catch` received.
 * @returns The name, or undefined where the value is no error.
 */
export function errorName(err: unknown): string | undefined {
    return err instanceof Error ? err.name : isRecord(err) ? stringOr(err.name) : undefined;
}

/**
 * A value when it is an instance of a class.
 * @param value Anything.
 * @param ctor The class it has to be an instance of.
 * @returns `value` narrowed to the class, or null.
 */
export function instanceOr<T>(value: unknown, ctor: abstract new (...args: never[]) => T): T | null {
    return value instanceof ctor ? value : null;
}

/**
 * Whether an event target is a DOM node. Read from the node's own `nodeType`,
 * so a node from another frame counts, as does an audit's stand-in for one.
 * @param target Anything, an event's target for one.
 * @returns True for a node.
 */
export function isNode(target: unknown): target is Node {
    return isRecord(target) && typeof target.nodeType === 'number';
}

/**
 * An event target where it is a DOM node.
 * @param target An event's target.
 * @returns The node, or null.
 */
export function nodeOr(target: unknown): Node | null {
    return isNode(target) ? target : null;
}

/**
 * Whether a value is an element, read from the `classList` and
 * `parentElement` every element has.
 * @param value Anything.
 * @returns True for an element.
 */
export function isElement(value: unknown): value is Element {
    return isRecord(value) && isRecord(value.classList) && 'parentElement' in value;
}

/**
 * Whether a value is an HTML element of a tag, read from its `tagName` as an
 * element of another frame has it too.
 * @param value Anything.
 * @param tag The tag, lowercase: `canvas`, `video`, `input`.
 * @returns True for an element of that tag.
 */
export function isTag<K extends keyof HTMLElementTagNameMap>(value: unknown, tag: K): value is HTMLElementTagNameMap[K] {
    return isRecord(value) && typeof value.tagName === 'string' && value.tagName.toLowerCase() === tag;
}

/**
 * The element with an id, when it has the tag the caller needs.
 * @param id The element's id.
 * @param tag The tag it has to have.
 * @returns The element, or null when there is none or it has another tag.
 */
export function elementByTag<K extends keyof HTMLElementTagNameMap>(id: string, tag: K): HTMLElementTagNameMap[K] | null {
    const el = document.getElementById(id);
    return isTag(el, tag) ? el : null;
}

/**
 * Parses JSON without trusting its shape.
 * @param text The JSON text.
 * @returns What it parses to, for the caller to check; undefined where it does not parse.
 */
export function parseJson(text: string | null | undefined): unknown {
    if (text === null || text === undefined) return undefined;
    try {
        const parsed: unknown = JSON.parse(text);
        return parsed;
    } catch {
        return undefined;
    }
}
