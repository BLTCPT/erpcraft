#!/usr/bin/env python3
"""Composite the demo video: narration.mp3 over a scrolling render
of demo-transcript.txt, timed by the cue sheet in script.md
(beats at 0:00 / 0:35 / 1:05 / 1:40 / 2:05). The transcript is the
real recorded session (run-demo.sh) — nothing is re-typed.

Renders one tall image of the whole transcript, crops a 1280x720
window per scroll position, and muxes with ffmpeg. Output:
erpcraft-demo.mp4 next to this script.

Usage: python3 composite.py
"""
import os, subprocess, sys
from PIL import Image, ImageDraw, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
TRANSCRIPT = os.path.join(HERE, "demo-transcript.txt")
NARRATION = os.path.join(HERE, "narration.mp3")
OUT = os.path.join(HERE, "erpcraft-demo.mp4")
FRAMES = os.path.join(HERE, ".frames")
DURATION = 149.808  # narration.mp3, measured with ffprobe

W, H = 1280, 720
MARGIN = 24
FONT_SIZE = 16
LINE_H = 27
VISIBLE = (H - 2 * MARGIN) // LINE_H
BG = (13, 17, 23)
FG = (230, 237, 243)
GREEN = (126, 231, 135)
BLUE = (121, 192, 255)
DIM = (139, 148, 158)
MAX_CHARS = (W - 2 * MARGIN) // 9  # DejaVu Sans Mono @16px ≈ 9.63px/char; be safe

FONT_PATH = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
FONT_BIG_PATH = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"

def color_of(raw: str):
    if raw.startswith("$ "):
        return GREEN
    if raw.startswith("# ──"):
        return BLUE
    if raw.startswith("#") or raw.startswith("[exit"):
        return DIM
    return FG

def main():
    raw_lines = open(TRANSCRIPT).read().splitlines()
    # Wrap long lines (continuation indent), tracking where each raw
    # line starts in display space and where each step section begins.
    display = []          # (text, color)
    raw_start = []        # display index per raw line
    section_start = {}    # step number -> display index
    for i, raw in enumerate(raw_lines):
        raw_start.append(len(display))
        if raw.startswith("# ── Step"):
            section_start[int(raw.split("Step")[1].split("·")[0].strip())] = len(display)
        color = color_of(raw)
        if len(raw) <= MAX_CHARS:
            display.append((raw, color))
        else:
            chunks = [raw[j:j + MAX_CHARS - 2] for j in range(0, len(raw), MAX_CHARS - 2)]
            display.append((chunks[0], color))
            for c in chunks[1:]:
                display.append(("  " + c, DIM))
    total_display = len(display)
    print(f"transcript: {len(raw_lines)} raw lines -> {total_display} display lines; "
          f"sections at {section_start}")

    font = ImageFont.truetype(FONT_PATH, FONT_SIZE)
    tall = Image.new("RGB", (W, total_display * LINE_H + 2 * MARGIN), BG)
    draw = ImageDraw.Draw(tall)
    for idx, (text, color) in enumerate(display):
        draw.text((MARGIN, MARGIN + idx * LINE_H), text, font=font, fill=color)

    def top_for(line_idx: int) -> int:
        """Window top (px) that puts display line `line_idx` at the top."""
        return max(0, min(line_idx * LINE_H, tall.height - H))

    # Scroll schedule: (beat_start_s, beat_end_s, first_line, last_line)
    # Beats from script.md's cue sheet; step 5+6 share the last beat.
    s = section_start
    beats = [
        (0.0,   35.0,  0,        s[2] - 1),
        (35.0,  65.0,  s[2],     s[3] - 1),
        (65.0,  100.0, s[3],     s[4] - 1),
        (100.0, 125.0, s[4],     s[5] - 1),
        (125.0, DURATION, s[5], total_display - 1),
    ]
    def window_top_at(t: float) -> int:
        for (t0, t1, first, last) in beats:
            if t0 <= t < t1 or (t >= t1 and (t0, t1) == (beats[-1][0], beats[-1][1])):
                span = t1 - t0
                frac = (t - t0) / span
                # 12% dwell at each end of the beat, linear between.
                if frac < 0.12:
                    p = 0.0
                elif frac > 0.88:
                    p = 1.0
                else:
                    p = (frac - 0.12) / 0.76
                start_top = top_for(first)
                end_top = top_for(max(first, last - VISIBLE + 1))
                return int(start_top + (end_top - start_top) * p)
        return top_for(total_display - 1)

    os.makedirs(FRAMES, exist_ok=True)
    for old in os.listdir(FRAMES):
        os.remove(os.path.join(FRAMES, old))
    step = 0.2  # seconds per schedule sample
    samples = []
    t = 0.0
    while t < DURATION:
        samples.append((t, window_top_at(t)))
        t += step
    samples.append((DURATION, window_top_at(DURATION)))

    # Emit a frame only when the picture changes (scroll position or
    # the end-card overlay in the final 5 seconds).
    concat = []
    prev_key = None
    big_font = ImageFont.truetype(FONT_BIG_PATH, 30)
    for i, (t0, top) in enumerate(samples):
        end_card = t0 >= DURATION - 5.0
        key = (top, end_card)
        dur = (samples[i + 1][0] if i + 1 < len(samples) else DURATION) - t0
        if key == prev_key and concat:
            name, d = concat[-1]
            concat[-1] = (name, d + dur)
            continue
        frame = tall.crop((0, top, W, top + H)).copy()
        if end_card:
            d2 = ImageDraw.Draw(frame, "RGBA")
            d2.rectangle([0, H - 84, W, H], fill=(13, 17, 23, 235))
            d2.text((W // 2, H - 58), "github.com/BLTCPT/erpcraft",
                    font=big_font, fill=(126, 231, 135), anchor="mm")
        name = f"f{len(concat):04d}.png"
        frame.save(os.path.join(FRAMES, name))
        concat.append((name, dur))
        prev_key = key
    with open(os.path.join(FRAMES, "concat.txt"), "w") as f:
        for name, dur in concat:
            f.write(f"file '{name}'\nduration {dur:.3f}\n")
        f.write(f"file '{concat[-1][0]}'\n")
    print(f"frames: {len(concat)} distinct pictures over {DURATION}s")

    cmd = [
        "ffmpeg", "-y", "-v", "error",
        "-f", "concat", "-safe", "0", "-i", os.path.join(FRAMES, "concat.txt"),
        "-i", NARRATION,
        "-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "20", "-r", "30",
        "-c:a", "aac", "-b:a", "128k",
        "-movflags", "+faststart", "-t", str(DURATION),
        OUT,
    ]
    subprocess.run(cmd, check=True)
    print(f"wrote {OUT} ({os.path.getsize(OUT)} bytes)")

if __name__ == "__main__":
    sys.exit(main())
