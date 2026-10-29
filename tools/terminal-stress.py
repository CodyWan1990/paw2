#!/usr/bin/env python3
"""Redraw many terminal rows at once, to look for compositor flicker.

Fills the bottom rows (where a hidden dock sits) with spinners, progress bars
and coloured noise, redrawn every frame. Watch for flickering rows, a dock that
flashes, or a stuttering cursor. Ctrl+C stops it.

    tools/terminal-stress.py            # bottom 20 rows at 60 fps
    tools/terminal-stress.py --full     # the whole terminal
    tools/terminal-stress.py --fps 0    # as fast as the terminal takes it
"""

import argparse
import random
import shutil
import sys
import time

SPINNER = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"
GLYPHS = "▁▂▃▄▅▆▇█░▒▓◐◓◑◒●○◆◇★☆"


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--full", action="store_true", help="redraw every row")
    parser.add_argument("--rows", type=int, default=20, help="rows to redraw (default 20)")
    parser.add_argument("--fps", type=float, default=60, help="frames a second, 0 for no limit")
    args = parser.parse_args()

    cols, rows = shutil.get_terminal_size()
    lines = rows - 1 if args.full else min(args.rows, rows - 2)
    top = rows - lines + 1
    bar_width = max(10, cols // 3)
    delay = 1 / args.fps if args.fps > 0 else 0

    write = sys.stdout.write
    write("\x1b[?25l\x1b[2J")  # hide the cursor, clear the screen
    frame = 0
    try:
        while True:
            for row in range(lines):
                percent = (frame * (row + 1)) % 101
                fill = bar_width * percent // 100
                noise = "".join(
                    random.choice(GLYPHS) for _ in range(max(0, cols - bar_width - 16))
                )
                colour = 31 + (row + frame // 5) % 7
                write(
                    f"\x1b[{top + row};1H\x1b[{colour}m"
                    f"{SPINNER[(frame + row) % len(SPINNER)]} "
                    f"[{'█' * fill}{' ' * (bar_width - fill)}] {percent:3d}% "
                    f"{noise}\x1b[0m\x1b[K"
                )
            sys.stdout.flush()
            frame += 1
            time.sleep(delay)
    except KeyboardInterrupt:
        pass
    finally:
        write("\x1b[0m\x1b[?25h\x1b[2J\x1b[H")  # reset colours, show the cursor


if __name__ == "__main__":
    main()
