# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

import argparse
import sys
import os
if __name__ == "__main__" and __package__ is None:
    current_script_dir = os.path.dirname(os.path.abspath(__file__))
    package_container_dir = os.path.dirname(current_script_dir)
    if package_container_dir not in sys.path:
        sys.path.insert(0, package_container_dir)

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--mode', default=os.environ.get("SELKIES_MODE", "webrtc"),
                        help="Specify the mode: 'webrtc' or 'websockets'; defaults to webrtc")
    args, _ = parser.parse_known_args()
    if args.mode == 'webrtc':
      from .legacy.webrtc import wr_entrypoint
      wr_entrypoint()
    else:
      from .selkies import ws_entrypoint
      ws_entrypoint()

if __name__ == "__main__":
    main()
