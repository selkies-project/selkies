/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The type of the stand-ins the audits build for the browser. An audit runs
 * a web-core module in Node over objects holding only the members that
 * module touches; `StandIn` says such an object is a structural subset of
 * the interface it stands in for, so a member it does carry has the real
 * member's type, and it crosses to the real interface where the module takes
 * it through `standIn` (`standIn<HTMLElement>(fake)`), the one place the
 * audits hand a module something no check can prove is what it asks for.
 * @module
 */
import { isRecord } from '../../web/selkies-web-core/lib/narrow.ts';

/**
 * Any of the members of `T`, each of the real member's type or a stand-in
 * for it; a function takes the real one's arguments and returns a stand-in
 * for its result, or nothing where the module drops the result, a promise
 * settles to one, and a list (an array, a `TouchList`) is anything indexable
 * that holds them. A function compares its parameters both ways, as the
 * DOM's own methods do, so a stand-in may take the one kind of listener or
 * option its audit passes. The DOM's types refer to each other without end,
 * so past six levels a member is the real type, and a stand-in nested deeper
 * than that is named where it is built.
 */
export type StandIn<T, Depth extends unknown[] = []> = Depth['length'] extends 6
    ? T
    : T extends (...args: infer A) => infer R
        ? { method(...args: A): StandIn<R, [...Depth, unknown]> | void }['method']
        : T extends Promise<infer U>
            ? Promise<StandIn<U, [...Depth, unknown]>>
            : T extends readonly (infer E)[]
                ? ArrayLike<StandIn<E, [...Depth, unknown]>>
                : T extends TouchList
                    ? ArrayLike<StandIn<Touch, [...Depth, unknown]>>
                    : T extends object ? { [K in keyof T]?: StandIn<T[K], [...Depth, unknown]> } : T;

/**
 * A stand-in where the module takes the interface it stands in for. A
 * stand-in holds only the members its audit's module touches, so no check can
 * show it is a `T`; this is where the audits say so, once, by name.
 * @param value The stand-in.
 * @returns The same object, typed as what it stands in for.
 */
export function standIn<T>(value: StandIn<T>): T;
export function standIn(value: unknown): unknown {
    return value;
}

/**
 * A module imported again under a query (`?variant=2`), so that it runs once
 * more over the globals an audit set up for it: its type is that of the module
 * imported by name, which a computed specifier does not carry, and the exports
 * the audit uses are checked to be there.
 * @param loaded What the import resolved to.
 * @param exports The exports the audit reads.
 * @returns The module, typed as the named one.
 * @throws Error where one of those exports is missing.
 */
export function freshModule<T>(loaded: unknown, exports: readonly (keyof T & string)[]): T {
    if (!hasExports<T>(loaded, exports)) throw new Error(`the module imported again lacks ${exports.join(', ')}`);
    return loaded;
}

/** Whether a loaded module has every export named. */
function hasExports<T>(loaded: unknown, exports: readonly (keyof T & string)[]): loaded is T {
    return isRecord(loaded) && exports.every((name) => name in loaded);
}

/**
 * An instance of a class made without running its constructor, for an audit
 * that sets up only the fields the methods under test read.
 * @param ctor The class.
 * @returns An object with the class's prototype and no fields, typed as `T`.
 */
export function bare<T extends object>(ctor: abstract new (...args: never[]) => object): T;
export function bare(ctor: { prototype: object | null }): unknown {
    const made: unknown = Object.create(ctor.prototype);
    return made;
}

/** One timer of an audit's virtual clock: what `setTimeout` was handed, and when it is due. */
export interface VirtualTimer {
    id: number;
    /** When it runs, on the audit's clock. */
    at: number;
    fn: () => void;
}
