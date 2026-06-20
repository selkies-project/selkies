/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Returns URL pathname against browser's URL even when running under
// iframe context where the pathname could be root directory `/` otherwise.
export function getRoutePrefix() {
  const pathname = window.location.pathname;
  const dirPath = pathname.substring(0, pathname.lastIndexOf('/') + 1);
  return dirPath.replace(/\/$/, '');
}

