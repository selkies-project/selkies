#!/usr/bin/env python3
"""The frame_num range a suite reads off the server log.

pixelflux logs the frame_num range an H.264 key frame carries each time it
changes (`frame_num_range_line` in crates/pixelflux/src/encoders/reference.rs),
and test_reference_invalidation reads the ranges a load ran on from those lines
(`helpers.frame_num_ranges`): the one in force when the load began, then each a
later key frame changed it to. A VA-API session changes it within one stream,
from the 65536 values of the set pixelflux writes to the 128 of the one
radeonsi writes.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

# The lines as pixelflux prints them, beside the lines around them in a log.
LOG = (
    "[X11] Stream settings active -> Res: 1280x720 | FPS: 60.0 | Encoder: VAAPI | Mode: H264\n"
    "[pixelflux] H.264 frame_num range: 65536 values.\n"
    "INFO:webrtc:Display 'primary' streaming\n"
)
LATER = "[pixelflux] H.264 frame_num range: 128 values.\n"


def main() -> "H.Results":
    res = H.Results("frame-num-range-log")
    text = LOG + LATER
    res.check("the range in force at the mark comes first, then each change after it",
              H.frame_num_ranges(text, len(LOG)) == [65536, 128],
              H.frame_num_ranges(text, len(LOG)))
    res.check("a mark past every line still names the range in force",
              H.frame_num_ranges(text, len(text)) == [128], H.frame_num_ranges(text, len(text)))
    res.check("a mark before every line names each range the log carries",
              H.frame_num_ranges(text, 0) == [65536, 128], H.frame_num_ranges(text, 0))
    res.check("a log naming no range names none", H.frame_num_ranges(LOG.splitlines()[0], 0) == [],
              H.frame_num_ranges(LOG.splitlines()[0], 0))
    return res


if __name__ == "__main__":
    sys.exit(0 if main().summary() else 1)
