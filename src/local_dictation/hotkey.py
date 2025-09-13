#!/usr/bin/env python3
"""
Optimized Hotkey Listener
- Fast ticker rate for responsive detection
- Efficient lock usage
"""
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
    # Single modifier keys (for single-key hotkeys)
    "FN": keyboard.Key.f13,  # F13 is often used as FN key on Mac
    "CAPS": keyboard.Key.caps_lock,
    "TAB": keyboard.Key.tab,
    "ESC": keyboard.Key.esc,
    "SPACE": keyboard.Key.space,
    "ENTER": keyboard.Key.enter,
    "BACKSPACE": keyboard.Key.backspace,
    "DELETE": keyboard.Key.delete,
    # Arrow keys
    "UP": keyboard.Key.up,
    "DOWN": keyboard.Key.down,
    "LEFT": keyboard.Key.left,
    "RIGHT": keyboard.Key.right,
    # Navigation keys
    "HOME": keyboard.Key.home,
    "END": keyboard.Key.end,
    "PAGEUP": keyboard.Key.page_up,
    "PAGEDOWN": keyboard.Key.page_down,
    # All Function keys
    "F1": keyboard.Key.f1,
    "F2": keyboard.Key.f2,
    "F3": keyboard.Key.f3,
    "F4": keyboard.Key.f4,
    "F5": keyboard.Key.f5,
    "F6": keyboard.Key.f6,
    "F7": keyboard.Key.f7,
    "F8": keyboard.Key.f8,
    "F9": keyboard.Key.f9,
    "F10": keyboard.Key.f10,
    "F11": keyboard.Key.f11,
    "F12": keyboard.Key.f12,
    "F13": keyboard.Key.f13,
    "F14": keyboard.Key.f14,
    "F15": keyboard.Key.f15,
    "F16": keyboard.Key.f16,
    "F17": keyboard.Key.f17,
    "F18": keyboard.Key.f18,
    "F19": keyboard.Key.f19,
    "F20": keyboard.Key.f20,
    # Special characters that people might want
    "GRAVE": keyboard.KeyCode.from_char('`'),  # Backtick
    "TILDE": keyboard.KeyCode.from_char('~'),
    "MINUS": keyboard.KeyCode.from_char('-'),
    "EQUALS": keyboard.KeyCode.from_char('='),
    "SEMICOLON": keyboard.KeyCode.from_char(';'),
    "QUOTE": keyboard.KeyCode.from_char("'"),
    "COMMA": keyboard.KeyCode.from_char(','),
    "PERIOD": keyboard.KeyCode.from_char('.'),
    "SLASH": keyboard.KeyCode.from_char('/'),
    "BACKSLASH": keyboard.KeyCode.from_char('\\'),
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
    Optimized Hotkey Listener
    - Fast ticker response (5ms)
    - Minimal latency design
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
        return int(time.perf_counter() * 1000)

    def _on_press(self, key):
        with self._lock:
            self._pressed.add(key)
            if not self._active and self.chord.issubset(self._pressed):
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
        # Fast tick rate for responsive release detection
        while not self._ticker_stop.is_set():
            with self._lock:
                if self._active and self._release_deadline:
                    if self._now_ms() >= self._release_deadline:
                        self._active = False
                        self._release_deadline = None
                        self.on_chord_active(False)
            time.sleep(0.005)  # 5ms vs 10ms for faster response