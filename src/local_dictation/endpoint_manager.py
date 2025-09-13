from __future__ import annotations

import time
from enum import Enum
from typing import Optional, Sequence

from .dictation_config import DictationConfig


class EndpointDecision(Enum):
    CONTINUE = 0
    SHOULD_STOP = 1


class EndpointManager:
    """Endpoint detection manager supporting push-to-talk or VAD auto-stop."""

    class Mode(Enum):
        PUSH_TO_TALK = "push_to_talk"
        AUTO_STOP_VAD = "auto_stop_vad"

    def __init__(self, cfg: DictationConfig, vad: Optional[object] = None):
        self.cfg = cfg
        self.mode = (self.Mode.PUSH_TO_TALK if cfg.use_push_to_talk
                     else self.Mode.AUTO_STOP_VAD)
        self.vad = vad
        self.last_speech_time: float = 0.0
        self.last_stop_time: float = 0.0
        self.is_speech: bool = False

    def on_frame(self, frame: Sequence[float]) -> EndpointDecision:
        """Process a single 20ms audio frame.

        Args:
            frame: Sequence of float samples (mono 16 kHz).
        """
        if self.mode == self.Mode.PUSH_TO_TALK:
            return EndpointDecision.CONTINUE

        if self.vad is None:
            return EndpointDecision.CONTINUE

        p = self.vad.get_speech_prob(frame)
        now = time.monotonic()
        if p >= self.cfg.vad_prob_threshold:
            self.is_speech = True
            self.last_speech_time = now
            return EndpointDecision.CONTINUE

        # speech ended? apply hangover + debounce
        if self.is_speech and (now - self.last_speech_time) * 1000 >= self.cfg.vad_hangover_ms:
            if (now - self.last_stop_time) * 1000 >= self.cfg.vad_debounce_ms:
                self.last_stop_time = now
                self.is_speech = False
                return EndpointDecision.SHOULD_STOP

        return EndpointDecision.CONTINUE
