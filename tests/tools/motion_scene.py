#!/usr/bin/env python3
"""A moving scene whose every frame says which frame it is.

Fills an X display with a solid background, a strip of squares spelling the
frame index in binary, a bar that steps along the width each frame and a
scrolling checkerboard band, all placed by the frame index alone. A viewer
that reads the index off a decoded picture can rebuild the frame it should be
looking at and count the pixels that disagree; a picture decoded against the
wrong reference disagrees everywhere the scene moved. A band of noise along
the bottom, fresh every frame so no motion search can predict it, costs the
encoder what real content does and keeps a constant-bitrate stream at its
configured rate; it is not rebuilt.

    motion_scene.py DISPLAY WIDTH HEIGHT FPS
"""
import random
import sys
import tkinter as tk

CODE_BITS = 16
CODE_SIZE = 48
CODE_GAP = 8
CODE_X = 16
CODE_Y = 16
BAND_Y = 96
BAND_H = 64
CHECK = 16
BAND_STEP = 4
BAR_W = 160
BAR_STEP = 12
NOISE_H = 240
NOISE_IMAGES = 4
BACKGROUND = "#1e2878"
BAR = "#dc3c28"


def geometry() -> dict:
    """The layout constants, for a checker that rebuilds the expected frame."""
    return {
        "codeBits": CODE_BITS, "codeSize": CODE_SIZE, "codeGap": CODE_GAP,
        "codeX": CODE_X, "codeY": CODE_Y, "bandY": BAND_Y, "bandH": BAND_H,
        "check": CHECK, "bandStep": BAND_STEP, "barW": BAR_W, "barStep": BAR_STEP,
        "noiseH": NOISE_H,
        "background": [0x1e, 0x28, 0x78], "bar": [0xdc, 0x3c, 0x28],
    }


def main(display: str, width: int, height: int, fps: int) -> None:
    root = tk.Tk(screenName=display)
    root.overrideredirect(True)
    root.geometry(f"{width}x{height}+0+0")
    canvas = tk.Canvas(root, width=width, height=height, bg=BACKGROUND,
                       highlightthickness=0)
    canvas.pack()

    period = 2 * CHECK
    band = tk.PhotoImage(width=width + period, height=BAND_H)
    band.put(" ".join(
        "{" + " ".join(
            "#ffffff" if ((x // CHECK) + (y // CHECK)) % 2 == 0 else "#000000"
            for x in range(width + period)) + "}"
        for y in range(BAND_H)), to=(0, 0))
    band_item = canvas.create_image(0, BAND_Y, image=band, anchor="nw")
    bar_bottom = height - NOISE_H
    bar_item = canvas.create_rectangle(0, BAND_Y + BAND_H, BAR_W, bar_bottom,
                                       fill=BAR, outline="")
    rng = random.Random(1)
    noises = []
    for _ in range(NOISE_IMAGES):
        noise = tk.PhotoImage(width=2 * width, height=NOISE_H)
        noise.put(" ".join(
            "{" + " ".join("#%06x" % rng.getrandbits(24) for _ in range(2 * width)) + "}"
            for _ in range(NOISE_H)), to=(0, 0))
        noises.append(noise)
    noise_item = canvas.create_image(0, bar_bottom, image=noises[0], anchor="nw")
    squares = [
        canvas.create_rectangle(
            CODE_X + i * (CODE_SIZE + CODE_GAP), CODE_Y,
            CODE_X + i * (CODE_SIZE + CODE_GAP) + CODE_SIZE, CODE_Y + CODE_SIZE,
            fill="#000000", outline="")
        for i in range(CODE_BITS)
    ]
    state = {"index": 0}
    interval_ms = max(1, round(1000 / fps))

    def tick() -> None:
        index = state["index"] = (state["index"] + 1) % (1 << CODE_BITS)
        for i, item in enumerate(squares):
            canvas.itemconfigure(
                item, fill="#ffffff" if (index >> i) & 1 else "#000000")
        canvas.coords(band_item, -((index * BAND_STEP) % period), BAND_Y)
        x = (index * BAR_STEP) % (width - BAR_W)
        canvas.coords(bar_item, x, BAND_Y + BAND_H, x + BAR_W, bar_bottom)
        canvas.itemconfigure(noise_item, image=noises[index % NOISE_IMAGES])
        canvas.coords(noise_item, -((index * 97) % width), bar_bottom)
        root.after(interval_ms, tick)

    root.after(interval_ms, tick)
    root.mainloop()


if __name__ == "__main__":
    main(sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]))
