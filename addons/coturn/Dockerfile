# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# This file incorporates work covered by the following copyright and
# permission notice:
#
#   Copyright 2019 Google LLC
#
#   Licensed under the Apache License, Version 2.0 (the "License");
#   you may not use this file except in compliance with the License.
#   You may obtain a copy of the License at
#
#        http://www.apache.org/licenses/LICENSE-2.0
#
#   Unless required by applicable law or agreed to in writing, software
#   distributed under the License is distributed on an "AS IS" BASIS,
#   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
#   See the License for the specific language governing permissions and
#   limitations under the License.

FROM coturn/coturn:alpine

USER 0

# apk has no retry option of its own, so the one package fetch this image makes
# is bounded-retried in the shell rather than left to fail the build outright.
RUN i=1; until apk add -u --no-cache bind-tools; do \
        [ "$i" -ge 5 ] && exit 1; \
        i=$((i+1)); sleep 5; \
    done

COPY detect_external_ip.sh /usr/bin/detect_external_ip
RUN chmod -f +x /usr/bin/detect_external_ip

COPY entrypoint.sh /
RUN chmod -f +x /entrypoint.sh

# nobody:nogroup by number, so a cluster that requires a non-root user can
# verify it from the image.
USER 65534:65533

ENTRYPOINT ["/entrypoint.sh"]
