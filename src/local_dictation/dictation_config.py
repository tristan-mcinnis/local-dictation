from __future__ import annotations
from dataclasses import dataclass


@dataclass
class DictationConfig:
    """Configuration for dictation endpointing and modes."""

    # Modes
    use_push_to_talk: bool = True
    use_auto_stop_vad: bool = False

    # VAD tuning (auto-stop mode)
    vad_frame_ms: int = 20          # 20 ms frames
    vad_sample_rate: int = 16000
    vad_hangover_ms: int = 150      # stop after 150 ms of non-speech
    vad_debounce_ms: int = 200      # ignore re-triggers within 200 ms
    vad_prob_threshold: float = 0.6 # silero speech prob threshold

    # Push-to-talk
    ptt_key: str = "shift"          # key name for pynput (e.g. 'shift')
