from __future__ import annotations

from typing import Callable

from pynput import keyboard

from .dictation_config import DictationConfig


class PTTController:
    """Simple push-to-talk controller using pynput."""

    def __init__(self, cfg: DictationConfig,
                 on_start: Callable[[], None],
                 on_stop: Callable[[], None]):
        self.cfg = cfg
        self._on_start = on_start
        self._on_stop = on_stop
        self._is_down = False
        self._listener = keyboard.Listener(
            on_press=self._on_press,
            on_release=self._on_release,
        )

    def start(self):
        self._listener.start()

    def stop(self):
        self._listener.stop()

    def _matches_key(self, key) -> bool:
        try:
            return key == getattr(keyboard.Key, self.cfg.ptt_key)
        except AttributeError:
            return False

    def _on_press(self, key):
        if not self._is_down and self._matches_key(key):
            self._is_down = True
            self._on_start()

    def _on_release(self, key):
        if self._is_down and self._matches_key(key):
            self._is_down = False
            self._on_stop()
