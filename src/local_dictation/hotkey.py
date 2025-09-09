#!/usr/bin/env python3
from __future__ import annotations
from pynput import keyboard
import threading
import time

# Mapping for chord parser
KEY_MAP = {
    "CMD": keyboard.Key.cmd,
    "ALT": keyboard.Key.alt,      # Option
    "OPT": keyboard.Key.alt,
    "CTRL": keyboard.Key.ctrl,
    "SHIFT": keyboard.Key.shift,
    # allow left/right synonyms
    "LALT": keyboard.Key.alt,
    "RALT": keyboard.Key.alt,
    "LCMD": keyboard.Key.cmd,
    "RCMD": keyboard.Key.cmd,
    "LCTRL": keyboard.Key.ctrl,
    "RCTRL": keyboard.Key.ctrl,
    "LSHIFT": keyboard.Key.shift,
    "RSHIFT": keyboard.Key.shift,
}

def parse_chord(chord_str: str | None) -> set:
    if not chord_str:
        return {keyboard.Key.cmd, keyboard.Key.alt}  # ⌘ + ⌥
    parts = [p.strip().upper() for p in chord_str.replace(",", " ").split() if p.strip()]
    chord = set()
    for p in parts:
        if len(p) == 1:
            chord.add(keyboard.KeyCode.from_char(p.lower()))
        elif p.startswith("F") and p[1:].isdigit():
            chord.add(getattr(keyboard.Key, p.lower()))
        else:
            chord.add(KEY_MAP.get(p, None) or getattr(keyboard.Key, p.lower(), None))
    return {k for k in chord if k is not None}

class HotkeyListener:
    """
    Global hotkey chord listener for macOS using pynput.
    Calls on_chord_active(True/False) when chord transitions pressed<->released.
    Debounces release to avoid bounce.
    """
    def __init__(self, chord: set, debounce_ms: int, on_chord_active):
        self.chord = chord
        self.debounce_ms = debounce_ms
        self.on_chord_active = on_chord_active

        self._pressed = set()
        self._active = False
        self._lock = threading.Lock()
        self._release_deadline = None

        self._listener = keyboard.Listener(on_press=self._on_press, on_release=self._on_release)

        self._ticker_stop = threading.Event()
        self._ticker = threading.Thread(target=self._tick, daemon=True)

    def start(self):
        self._listener.start()
        self._ticker.start()

    def join(self):
        self._listener.join()
        self._ticker_stop.set()
        self._ticker.join()

    def stop(self):
        self._listener.stop()
        self._ticker_stop.set()

    def _now_ms(self):
        return int(time.time() * 1000)

    def _on_press(self, key):
        with self._lock:
            self._pressed.add(key)
            if self.chord.issubset(self._pressed):
                if not self._active:
                    self._active = True
                    self._release_deadline = None
                    self.on_chord_active(True)

    def _on_release(self, key):
        with self._lock:
            self._pressed.discard(key)
            if self._active and not self.chord.issubset(self._pressed):
                # arm a delayed stop
                if self._release_deadline is None:
                    self._release_deadline = self._now_ms() + self.debounce_ms

    def _tick(self):
        while not self._ticker_stop.is_set():
            with self._lock:
                if self._active and self._release_deadline:
                    if self._now_ms() >= self._release_deadline:
                        self._active = False
                        self._release_deadline = None
                        self.on_chord_active(False)
            time.sleep(0.01)