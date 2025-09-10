#!/usr/bin/env python3
"""
Optimized Audio Recording
- Pre-allocated numpy arrays for efficiency
- Direct 16kHz recording when supported
- Cached resampling parameters
"""
from __future__ import annotations
import os
import sys
import math
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
    Tries to use 16kHz directly if supported
    """
    devs = sd.query_devices()
    selected_idx = None
    
    if preferred_device_name:
        for i, d in enumerate(devs):
            if d.get("max_input_channels", 0) > 0 and preferred_device_name.lower() in d["name"].lower():
                selected_idx = i
                break
    
    # Try to use 16kHz directly
    try:
        if selected_idx is not None:
            sd.check_input_settings(device=selected_idx, samplerate=WHISPER_SR)
        else:
            sd.check_input_settings(samplerate=WHISPER_SR)
        return WHISPER_SR, selected_idx
    except:
        # Fall back to native sample rate
        if selected_idx is None:
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
    Optimized Voice Recorder
    - Pre-allocated ring buffer for efficiency
    - Cached resampling parameters
    - Direct 16kHz recording when possible
    - Optional VAD for silence filtering
    """
    def __init__(self, device_name: str | None, max_sec: float, highpass_hz: float = 0.0, channels: int = 1, use_vad: bool = False):
        self.max_sec = max_sec
        self.highpass_hz = highpass_hz
        self.channels = channels
        self.use_vad = use_vad
        self.vad = None

        self.samplerate, self.device_index = pick_samplerate(device_name)
        sd.default.samplerate = self.samplerate
        sd.default.channels = channels
        if self.device_index is not None:
            sd.default.device = (self.device_index, None)

        # Pre-allocate ring buffer
        self.max_frames = int(self.max_sec * self.samplerate)
        self._buffer = np.zeros(self.max_frames, dtype=np.float32)
        self._write_pos = 0
        self._frames_written = 0
        self._stream = None
        self._active = False
        
        # Pre-compute resampling parameters
        self.needs_resample = (self.samplerate != WHISPER_SR)
        if self.needs_resample:
            g = math.gcd(int(self.samplerate), WHISPER_SR)
            self.resample_up = WHISPER_SR // g
            self.resample_down = int(self.samplerate) // g
        
        # Pre-compute high-pass filter if needed
        if self.highpass_hz and self.highpass_hz > 0:
            nyq = 0.5 * self.samplerate
            w = self.highpass_hz / nyq
            self.hp_b, self.hp_a = butter(1, w, "highpass")
        else:
            self.hp_b = self.hp_a = None
        
        # Initialize VAD if requested
        if self.use_vad:
            try:
                from .vad import SileroVAD
                self.vad = SileroVAD(threshold=0.5, min_speech_duration_ms=250, min_silence_duration_ms=100)
                print("✅ VAD enabled for silence filtering", file=sys.stderr)
            except Exception as e:
                print(f"⚠️ VAD initialization failed, continuing without: {e}", file=sys.stderr)
                self.vad = None

    def _callback(self, indata, frames, time, status):
        if status:
            print(f"[audio] {status}", file=sys.stderr)
        if not self._active:
            return
        
        # Extract mono, avoid copy when possible
        x = indata[:, 0] if indata.ndim > 1 else indata
        x = x.astype(np.float32, copy=False)
        
        # Write to circular buffer efficiently
        n = len(x)
        if self._write_pos + n <= self.max_frames:
            self._buffer[self._write_pos:self._write_pos + n] = x
            self._write_pos = (self._write_pos + n) % self.max_frames
        else:
            # Wrap around
            split = self.max_frames - self._write_pos
            self._buffer[self._write_pos:] = x[:split]
            self._buffer[:n - split] = x[split:]
            self._write_pos = n - split
        
        self._frames_written = min(self._frames_written + n, self.max_frames)

    def start(self):
        if self._active:
            return
        self._active = True
        self._write_pos = 0
        self._frames_written = 0
        self._buffer.fill(0)
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

        if self._frames_written == 0:
            print("(no speech captured)", file=sys.stderr)
            return None

        # Extract recorded audio from circular buffer
        if self._frames_written < self.max_frames:
            audio = self._buffer[:self._frames_written].copy()
        else:
            # Full buffer - reconstruct in correct order
            audio = np.concatenate([
                self._buffer[self._write_pos:],
                self._buffer[:self._write_pos]
            ])

        # Apply pre-computed high-pass filter
        if self.hp_b is not None:
            audio = lfilter(self.hp_b, self.hp_a, audio).astype(np.float32)

        # Apply pre-computed resampling if needed
        if self.needs_resample:
            print(f"🔄 Resampling {self.samplerate}→{WHISPER_SR}", file=sys.stderr)
            audio = resample_poly(audio, self.resample_up, self.resample_down).astype(np.float32)

        # Apply VAD filtering if enabled
        if self.vad is not None:
            original_len = len(audio) / WHISPER_SR
            audio = self.vad.filter_audio(audio, sample_rate=WHISPER_SR)
            if len(audio) > 0:
                filtered_len = len(audio) / WHISPER_SR
                reduction_pct = (1 - filtered_len / original_len) * 100
                print(f"🔇 VAD filtered {reduction_pct:.0f}% silence ({original_len:.1f}s → {filtered_len:.1f}s)", file=sys.stderr)
            else:
                print("🔇 VAD: No speech detected", file=sys.stderr)
                return None

        return audio