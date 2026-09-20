#!/usr/bin/env python3
"""Generate the tiny, original Local Dictation feedback cues.

The cues use quiet sine partials with short equal-power ramps.  Keeping this
generator checked in makes the binary assets reproducible without external
audio tools or copyrighted source material.
"""

from __future__ import annotations

import math
import struct
import wave
from pathlib import Path

RATE = 48_000
OUT = Path(__file__).resolve().parents[2] / "resources" / "sounds"


def tone(seconds: float, notes: list[tuple[float, float, float]], gain: float = 0.13) -> list[float]:
    """Synthesize (frequency, start, duration) notes into a quiet mono cue."""
    length = int(seconds * RATE)
    samples = [0.0] * length
    for frequency, start, duration in notes:
        first = int(start * RATE)
        count = min(int(duration * RATE), length - first)
        for offset in range(max(0, count)):
            t = offset / RATE
            # 7 ms attack and 24 ms release keep the cues click-free.
            attack = min(1.0, t / 0.007)
            release = min(1.0, max(0.0, (duration - t) / 0.024))
            envelope = attack * release
            fundamental = math.sin(2 * math.pi * frequency * t)
            partial = 0.16 * math.sin(2 * math.pi * frequency * 2 * t)
            samples[first + offset] += gain * envelope * (fundamental + partial)
    return [max(-0.9, min(0.9, sample)) for sample in samples]


def write(name: str, samples: list[float]) -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    with wave.open(str(OUT / name), "wb") as file:
        file.setnchannels(1)
        file.setsampwidth(2)
        file.setframerate(RATE)
        file.writeframes(b"".join(struct.pack("<h", int(sample * 32767)) for sample in samples))


def main() -> None:
    # Each cue has a distinct contour, not a stock system-sound imitation:
    # start rises, stop resolves downward, and errors use a restrained minor
    # interval rather than an alarm-like blast.
    write("start.wav", tone(0.115, [(620, 0.000, 0.048), (830, 0.046, 0.069)]))
    write("stop.wav", tone(0.125, [(760, 0.000, 0.052), (520, 0.044, 0.081)]))
    # The landed cue answers the start cue with the same notes in reverse. It is
    # equally loud, but slightly longer and descending, so it reads as a clear
    # completion rather than a second "start" or a sharp notification.
    write("success.wav", tone(0.140, [(830, 0.000, 0.060), (620, 0.052, 0.088)]))
    write("cancel.wav", tone(0.135, [(300, 0.000, 0.060), (220, 0.052, 0.083)], 0.11))
    write("latch.wav", tone(0.090, [(540, 0.000, 0.075)], 0.105))
    write("error.wav", tone(0.145, [(260, 0.000, 0.068), (195, 0.064, 0.078)], 0.105))


if __name__ == "__main__":
    main()
