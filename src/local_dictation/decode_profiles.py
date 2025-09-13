from __future__ import annotations

from dataclasses import dataclass
from enum import Enum


class Strategy(Enum):
    GREEDY = "greedy"
    BEAM = "beam"


@dataclass
class WhisperParams:
    use_gpu: bool = True
    coreml_encode: bool = True
    no_timestamps: bool = True
    temperature: float = 0.0
    strategy: Strategy = Strategy.GREEDY
    beam_size: int = 1


def params_for(profile: str) -> WhisperParams:
    """Return Whisper parameter presets for a profile."""
    if profile == "longform":
        return WhisperParams(
            use_gpu=True,
            coreml_encode=True,
            no_timestamps=False,
            temperature=0.0,
            strategy=Strategy.BEAM,
            beam_size=4,
        )
    # default to burst profile
    return WhisperParams(
        use_gpu=True,
        coreml_encode=True,
        no_timestamps=True,
        temperature=0.0,
        strategy=Strategy.GREEDY,
        beam_size=1,
    )
