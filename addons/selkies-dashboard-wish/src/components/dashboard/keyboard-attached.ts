/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import * as React from "react";
import { hardwareKeyboard } from "@/utils";

/** The page's attached-keyboard verdict, listening from load. */
export const keyboardWatch = hardwareKeyboard();

/** Whether a hardware keyboard is attached, which keeps the system's on-screen one down. */
export function useKeyboardAttached(): boolean {
    return React.useSyncExternalStore(keyboardWatch.subscribe, keyboardWatch.attached);
}
