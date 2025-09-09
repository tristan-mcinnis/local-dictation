#!/usr/bin/env python3
from __future__ import annotations
import os
import sys
import math
from collections import deque
import numpy as np
import sounddevice as sd
from scipy.signal import resample_poly, butter, lfilter

WHISPER_SR = 16000

def list_input_devices() -> list[dict]:
    devices = []
    for idx, d in enumerate(sd.query_devices()):
        if d.get("max_input_channels", 0) > 0:
            devices.append({"index": idx, "name": d["name"], "default_samplerate": d["default_samplerate"]})
    return devices

def pick_samplerate(preferred_device_name: str | None) -> tuple[int, int | None]:
    """
    Returns (samplerate, device_index)
    If preferred substring matches, select that device.
    """
    devs = sd.query_devices()
    selected_idx = None
    if preferred_device_name:
        for i, d in enumerate(devs):
            if d.get("max_input_channels", 0) > 0 and preferred_device_name.lower() in d["name"].lower():
                selected_idx = i
                break
    if selected_idx is None:
        # default input device
        try:
            info = sd.query_devices(kind="input")
            sr = int(info["default_samplerate"])
            return sr, None
        except Exception:
            return WHISPER_SR, None
    sr = int(devs[selected_idx]["default_samplerate"])
    return sr, selected_idx

class VoiceRecorder:
    """
    Records mono float32 audio while active=True.
    Keeps a bounded ring buffer of last `max_sec`.
    On stop(), returns float32 mono at 16kHz (polyphase-resampled).
    """
    def __init__(self, device_name: str | None, max_sec: float, highpass_hz: float = 0.0, channels: int = 1):
        self.max_sec = max_sec
        self.highpass_hz = highpass_hz
        self.channels = channels

        self.samplerate, self.device_index = pick_samplerate(device_name)
        sd.default.samplerate = self.samplerate
        sd.default.channels = channels
        if self.device_index is not None:
            sd.default.device = (self.device_index, None)

        self.max_frames = int(self.max_sec * self.samplerate)
        self._buf = deque()
        self._frames = 0
        self._stream = None
        self._active = False

    def _callback(self, indata, frames, time, status):
        if status:
            print(f"[audio] {status}", file=sys.stderr)
        if not self._active:
            return
        # ensure mono float32 in [-1,1]
        x = indata.astype(np.float32, copy=False)
        if x.ndim > 1:
            x = x[:, 0:1]  # take first channel, keep 2D
        self._buf.append(x.copy())
        self._frames += x.shape[0]
        # trim head if exceeding window
        while self._frames > self.max_frames and self._buf:
            old = self._buf.popleft()
            self._frames -= old.shape[0]

    def start(self):
        if self._active:
            return
        self._active = True
        self._buf.clear()
        self._frames = 0
        self._stream = sd.InputStream(callback=self._callback)
        self._stream.start()
        print("🎤 Recording...", file=sys.stderr)

    def stop(self) -> np.ndarray | None:
        if not self._active:
            return None
        self._active = False
        if self._stream:
            self._stream.stop()
            self._stream.close()
            self._stream = None

        if not self._buf:
            print("(no speech captured)", file=sys.stderr)
            return None

        audio = np.concatenate(list(self._buf), axis=0).reshape(-1).astype(np.float32, copy=False)

        # optional high-pass
        if self.highpass_hz and self.highpass_hz > 0:
            nyq = 0.5 * self.samplerate
            w = self.highpass_hz / nyq
            b, a = butter(1, w, "highpass")
            audio = lfilter(b, a, audio).astype(np.float32, copy=False)

        # resample to 16k via polyphase
        if self.samplerate != WHISPER_SR:
            g = math.gcd(int(self.samplerate), WHISPER_SR)
            up, down = WHISPER_SR // g, int(self.samplerate) // g
            print(f"🔄 Resampling {self.samplerate}→{WHISPER_SR}", file=sys.stderr)
            audio = resample_poly(audio, up, down).astype(np.float32, copy=False)

        return audio