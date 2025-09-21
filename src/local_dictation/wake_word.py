"""Wake word detection for hands-free dictation on macOS."""

from __future__ import annotations

import math
import queue
import threading
import time
from dataclasses import dataclass
from difflib import SequenceMatcher
from typing import Callable, Iterable, List, Optional

import numpy as np
import sounddevice as sd
from scipy.signal import resample_poly

from .audio import WHISPER_SR, pick_samplerate
from .vad import SileroVAD

import logging

logger = logging.getLogger(__name__)


@dataclass
class WakeWordConfig:
    words: List[str]
    window_seconds: float = 2.5
    min_gap_seconds: float = 2.0
    vad_threshold: float = 0.55
    match_threshold: float = 0.78


class WakeWordDetector:
    """Background listener that detects wake words and triggers a callback."""

    def __init__(
        self,
        transcriber,
        config: WakeWordConfig,
        *,
        device_name: Optional[str] = None,
        on_detect: Optional[Callable[[], None]] = None,
        transcribe_lock: Optional[threading.Lock] = None,
    ) -> None:
        self._transcriber = transcriber
        self._config = config
        self._wake_words = [word.lower() for word in config.words if word]
        self._on_detect = on_detect
        self._transcribe_lock = transcribe_lock or threading.Lock()

        self._stop_event = threading.Event()
        self._paused = threading.Event()
        self._segment_frames: List[np.ndarray] = []
        self._buffer = np.zeros(0, dtype=np.float32)
        self._queue: queue.Queue[Optional[np.ndarray]] = queue.Queue(maxsize=8)
        self._worker: Optional[threading.Thread] = None
        self._stream: Optional[sd.InputStream] = None

        input_samplerate, device_index = pick_samplerate(device_name)
        self._device_index = device_index
        self._input_samplerate = int(input_samplerate)
        self._target_samplerate = WHISPER_SR
        self._needs_resample = self._input_samplerate != self._target_samplerate
        if self._needs_resample:
            g = math.gcd(self._input_samplerate, self._target_samplerate)
            self._resample_up = self._target_samplerate // g
            self._resample_down = self._input_samplerate // g
        else:
            self._resample_up = 1
            self._resample_down = 1

        self._frame_samples = int(self._target_samplerate * 0.03)
        self._silence_timeout = 0.35
        self._last_speech_time = 0.0
        self._segment_active = False
        self._last_detect = 0.0

        self._vad = SileroVAD(
            threshold=config.vad_threshold,
            min_speech_duration_ms=200,
            min_silence_duration_ms=int(self._silence_timeout * 1000),
        )

    def start(self) -> None:
        if self._stream is not None:
            return

        self._stop_event.clear()
        self._paused.clear()
        self._worker = threading.Thread(target=self._process_segments, daemon=True)
        self._worker.start()

        stream_kwargs = {
            "samplerate": self._input_samplerate,
            "channels": 1,
            "dtype": "float32",
            "callback": self._audio_callback,
            "blocksize": self._frame_samples,
        }
        if self._device_index is not None:
            stream_kwargs["device"] = (self._device_index, None)

        try:
            self._stream = sd.InputStream(**stream_kwargs)
            self._stream.start()
            logger.info("Wake word listener active (%s)", ", ".join(self._wake_words))
        except Exception as exc:  # pragma: no cover - hardware dependent
            logger.error("Failed to start wake word listener: %s", exc)
            self.stop()

    def pause(self) -> None:
        self._paused.set()
        self._buffer = np.zeros(0, dtype=np.float32)
        self._segment_frames.clear()

    def resume(self) -> None:
        self._paused.clear()
        self._buffer = np.zeros(0, dtype=np.float32)
        self._segment_frames.clear()
        self._segment_active = False
        self._last_speech_time = 0.0
        self._last_detect = time.monotonic()

    def stop(self) -> None:
        self._stop_event.set()
        self._paused.set()
        if self._stream is not None:
            try:
                self._stream.stop()
                self._stream.close()
            except Exception:  # pragma: no cover - cleanup best effort
                pass
            self._stream = None
        try:
            self._queue.put_nowait(None)
        except queue.Full:
            pass
        if self._worker is not None:
            self._worker.join(timeout=2)
            self._worker = None

    def _audio_callback(self, indata, frames, time_info, status) -> None:
        if status:  # pragma: no cover - hardware dependent
            logger.debug("Wake word stream status: %s", status)

        if self._paused.is_set():
            return

        audio = indata[:, 0].astype(np.float32, copy=False)
        if self._needs_resample:
            audio = resample_poly(audio, self._resample_up, self._resample_down).astype(np.float32, copy=False)

        self._buffer = np.concatenate([self._buffer, audio])

        while len(self._buffer) >= self._frame_samples:
            frame = self._buffer[: self._frame_samples]
            self._buffer = self._buffer[self._frame_samples :]
            prob = self._vad.get_speech_prob(frame)
            now = time.monotonic()

            if prob >= self._config.vad_threshold:
                self._segment_frames.append(frame.copy())
                self._last_speech_time = now
                self._segment_active = True
            elif self._segment_active and (now - self._last_speech_time) >= self._silence_timeout:
                segment = np.concatenate(self._segment_frames) if self._segment_frames else np.array([], dtype=np.float32)
                self._segment_frames.clear()
                self._segment_active = False
                if segment.size and (len(segment) / self._target_samplerate) <= self._config.window_seconds:
                    try:
                        self._queue.put_nowait(segment)
                    except queue.Full:
                        pass

    def _process_segments(self) -> None:
        while not self._stop_event.is_set():
            try:
                segment = self._queue.get(timeout=0.2)
            except queue.Empty:
                continue

            if segment is None:
                break

            if segment.size == 0:
                continue

            if (segment.size / self._target_samplerate) < 0.15:
                continue

            try:
                with self._transcribe_lock:
                    text = self._transcriber.transcribe(segment, output="text")
            except Exception as exc:  # pragma: no cover - depends on transcriber
                logger.error("Wake word transcription failed: %s", exc)
                continue

            if self._matches(text):
                now = time.monotonic()
                if now - self._last_detect < self._config.min_gap_seconds:
                    continue
                self._last_detect = now
                if self._on_detect:
                    try:
                        self._on_detect()
                    except Exception:  # pragma: no cover - callback defined by caller
                        logger.exception("Wake word callback raised an error")

    def _matches(self, text: str) -> bool:
        if not text:
            return False

        normalized = text.lower().strip()
        if not normalized:
            return False

        candidates: List[str] = [normalized]
        candidates.extend(segment.strip() for segment in normalized.splitlines() if segment.strip())
        for candidate in candidates:
            for wake in self._wake_words:
                if wake in candidate:
                    return True
                if SequenceMatcher(None, candidate, wake).ratio() >= self._config.match_threshold:
                    return True
        return False


def build_wake_word_config(words: Iterable[str], **overrides: float) -> WakeWordConfig:
    """Helper to create a wake word configuration."""

    cleaned = [word.strip() for word in words if word and word.strip()]
    config = WakeWordConfig(words=cleaned)
    for key, value in overrides.items():
        if hasattr(config, key) and isinstance(value, (int, float)):
            setattr(config, key, float(value))
    return config

