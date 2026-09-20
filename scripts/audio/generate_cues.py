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


def landing_thud(seconds: float = 0.145) -> list[float]:
    """A soft, weighty impact: pitched body plus a very short felt-like tap."""
    length = int(seconds * RATE)
    samples: list[float] = []
    noise_state = 0x4C44  # fixed seed: the asset must be byte-reproducible
    phase = 0.0
    for index in range(length):
        t = index / RATE
        # A fast downward pitch bend gives the impact weight without sounding
        # like a bass note. The long exponential tail makes it feel as though
        # the text has settled into place.
        frequency = 132.0 + 138.0 * math.exp(-t / 0.026)
        phase += 2 * math.pi * frequency / RATE
        attack = min(1.0, t / 0.003)
        body = 0.16 * attack * math.exp(-t / 0.040) * (
            math.sin(phase) + 0.22 * math.sin(2 * phase)
        )

        # A deterministic, quickly damped tap supplies the physical edge of a
        # muted wooden/felt impact. It is deliberately quieter than the body.
        noise_state = (1103515245 * noise_state + 12345) & 0x7FFFFFFF
        noise = noise_state / 0x3FFFFFFF - 1.0
        tap = 0.026 * noise * math.exp(-t / 0.006)
        samples.append(max(-0.9, min(0.9, body + tap)))
    return samples


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
    # Completion is a soft, low thud: text landing in place, not another chime.
    write("success.wav", landing_thud())
    write("cancel.wav", tone(0.135, [(300, 0.000, 0.060), (220, 0.052, 0.083)], 0.11))
    write("latch.wav", tone(0.090, [(540, 0.000, 0.075)], 0.105))
    write("error.wav", tone(0.145, [(260, 0.000, 0.068), (195, 0.064, 0.078)], 0.105))


if __name__ == "__main__":
    main()
