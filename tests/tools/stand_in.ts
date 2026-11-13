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
 * it (`fake as StandIn<HTMLElement> as HTMLElement`), never through
 * `unknown`. Types only: nothing here exists at run time.
 * @module
 */

/**
 * Any of the members of `T`, each of the real member's type or a stand-in
 * for it; a function takes the real one's arguments and returns a stand-in
 * for its result, or nothing where the module drops the result, and a
 * promise settles to one. A function compares its parameters both ways, as
 * the DOM's own methods do, so a stand-in may take the one kind of listener
 * or option its audit passes. The DOM's types refer to each other without
 * end, so past six levels a member is the real type, and a stand-in nested
 * deeper than that is named where it is built.
 */
export type StandIn<T, Depth extends unknown[] = []> = Depth['length'] extends 6
    ? T
    : T extends (...args: infer A) => infer R
        ? { method(...args: A): StandIn<R, [...Depth, unknown]> | void }['method']
        : T extends Promise<infer U>
            ? Promise<StandIn<U, [...Depth, unknown]>>
            : T extends object ? { [K in keyof T]?: StandIn<T[K], [...Depth, unknown]> } : T;
