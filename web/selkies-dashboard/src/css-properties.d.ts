/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * CSS custom properties in React's `style` prop. A component hands the
 * stylesheet a value through a `--name` property that its rules read with
 * `var()`, which React's own typings leave out.
 * @module
 */
import "react";

declare module "react" {
  interface CSSProperties {
    /** A custom property, read by the stylesheet with `var(--name)`. */
    [property: `--${string}`]: string | number | undefined;
  }
}
