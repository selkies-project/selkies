# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Generate the Selkies wallpaper SVG: the logo centered on black.

Usage: python3 gen_wallpaper.py <logo.svg> <wallpaper.svg>

Plain SVG 1.1 only, since Qt's SVG image plugin rasterizes it; the full
3840x2160 canvas keeps the logo the same fraction of any screen under
pcmanfm-qt's zoom mode.
"""
import re
import sys

W, H = 3840, 2160
LOGO_SCALE = 1.4348  # 460-unit art -> ~660 px on the 2160 canvas

logo_src = open(sys.argv[1]).read()
logo_d = re.search(r'<path[^>]*\bd="([^"]+)"', logo_src).group(1)

tx = W / 2 - 230 * LOGO_SCALE
ty = H / 2 - 230 * LOGO_SCALE
out = f'''<?xml version="1.0" encoding="UTF-8"?>
<!-- This Source Code Form is subject to the terms of the Mozilla Public
     License, v. 2.0. If a copy of the MPL was not distributed with this
     file, You can obtain one at https://mozilla.org/MPL/2.0/.

     Generated from docs/assets/logo/selkies.svg by gen_wallpaper.py. -->
<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}">
  <defs>
    <linearGradient id="selkies-gradient" x1="0" y1="1" x2="0" y2="0">
      <stop offset="0.4293" stop-color="#D5499A"/>
      <stop offset="0.6263" stop-color="#C34AA2"/>
      <stop offset="0.9945" stop-color="#7967C5"/>
    </linearGradient>
  </defs>
  <rect x="0" y="0" width="{W}" height="{H}" fill="#000000"/>
  <g transform="translate({tx:.1f},{ty:.1f}) scale({LOGO_SCALE})">
    <g transform="translate(0,460) scale(0.1,-0.1)">
      <path fill="url(#selkies-gradient)" fill-rule="evenodd" d="{logo_d}"/>
    </g>
  </g>
</svg>
'''
open(sys.argv[2], 'w').write(out)
print(f'wrote {sys.argv[2]}')
