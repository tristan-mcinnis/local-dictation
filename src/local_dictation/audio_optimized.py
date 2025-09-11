#!/usr/bin/env python3
"""
Optimized Audio Recording with Lock-Free Ring Buffer
- Direct 16kHz mono capture
- Lock-free ring buffer for minimal latency
- WebRTC VAD integration for hands-free mode
- Pre-allocated buffers
"""
from __future__ import annotations
import numpy as np
import sounddevice as sd
import threading
from typing import Optional, Callable, Tuple
import sys
import queue
from .vad_webrtc import WebRTCVAD

WHISPER_SR = 16000

def list_input_devices() -> list[dict]:
    """List available input devices"""
    devices = []
    for idx, d in enumerate(sd.query_devices()):
        if d.get("max_input_channels", 0) > 0:
            devices.append({
                "index": idx,
                "name": d["name"],
                "default_samplerate": d["default_samplerate"]
            })
    return devices

def pick_device(preferred_name: str | None) -> Tuple[int, Optional[int]]:
    """
    Select audio device, preferring 16kHz support
    Returns (samplerate, device_index)
    """
    devices = sd.query_devices()
    selected_idx = None
    
    # Find device by name if specified
    if preferred_name:
        for i, d in enumerate(devices):
            if d.get("max_input_channels", 0) > 0:
                if preferred_name.lower() in d["name"].lower():
                    selected_idx = i
                    break
    
    # Always use 16kHz for minimal latency
    return WHISPER_SR, selected_idx

class OptimizedVoiceRecorder:
    """
    Optimized voice recorder with lock-free ring buffer
    - Direct 16kHz mono capture
    - Lock-free operations for minimal latency
    - Integrated VAD for hands-free mode
    """
    
    def __init__(self,
                 device_name: Optional[str] = None,
                 max_sec: float = 90.0,
                 buffer_ms: int = 20,
                 use_vad: bool = False,
                 vad_config: Optional[dict] = None):
        """
        Initialize optimized recorder
        
        Args:
            device_name: Substring of device name to use
            max_sec: Maximum recording duration
            buffer_ms: Buffer size in milliseconds (10-20ms optimal)
            use_vad: Enable VAD for hands-free mode
            vad_config: VAD configuration parameters
        """
        self.max_sec = max_sec
        self.buffer_ms = buffer_ms
        self.use_vad = use_vad
        
        # Audio device setup - always 16kHz mono
        self.samplerate, self.device_index = pick_device(device_name)
        self.channels = 1  # Always mono for efficiency
        
        # Configure sounddevice
        sd.default.samplerate = self.samplerate
        sd.default.channels = self.channels
        if self.device_index is not None:
            sd.default.device = (self.device_index, None)
        
        # Buffer configuration
        self.buffer_samples = int(self.samplerate * buffer_ms / 1000)
        self.max_samples = int(self.max_sec * self.samplerate)
        
        # Pre-allocate lock-free ring buffer
        self._ring_buffer = np.zeros(self.max_samples, dtype=np.float32)
        self._write_pos = 0
        self._read_pos = 0
        self._samples_available = 0
        
        # Stream state
        self._stream = None
        self._active = False
        self._recording = False
        
        # VAD setup
        self.vad = None
        if use_vad:
            vad_config = vad_config or {}
            self.vad = WebRTCVAD(
                aggressiveness=vad_config.get('aggressiveness', 2),
                frame_duration_ms=vad_config.get('frame_ms', 20),
                hangover_ms=vad_config.get('hangover_ms', 300),
                min_utterance_ms=vad_config.get('min_utterance_ms', 300),
                max_utterance_ms=vad_config.get('max_utterance_ms', 10000)
            )
        
        # Callbacks for hands-free mode
        self.on_voice_start: Optional[Callable[[], None]] = None
        self.on_voice_end: Optional[Callable[[], None]] = None
        
        # Performance metrics
        self._last_callback_time = 0
        self._callback_count = 0
    
    def _audio_callback(self, indata, frames, time_info, status):
        """
        Low-latency audio callback
        - Lock-free ring buffer write
        - Optional VAD processing
        """
        if status:
            print(f"[audio] {status}", file=sys.stderr)
        
        if not self._active:
            return
        
        # Extract mono channel efficiently (no copy if already mono)
        audio = indata[:, 0] if indata.ndim > 1 else indata.ravel()
        audio = audio.astype(np.float32, copy=False)
        
        # Lock-free ring buffer write
        n = len(audio)
        write_pos = self._write_pos
        
        # Check available space (lock-free)
        space_available = self.max_samples - self._samples_available
        if n > space_available:
            # Buffer full, overwrite oldest data
            n = space_available
        
        # Write to ring buffer (no locks needed)
        if write_pos + n <= self.max_samples:
            # Simple case: no wrap
            self._ring_buffer[write_pos:write_pos + n] = audio[:n]
            new_write_pos = write_pos + n
        else:
            # Wrap around
            first_part = self.max_samples - write_pos
            self._ring_buffer[write_pos:] = audio[:first_part]
            self._ring_buffer[:n - first_part] = audio[first_part:n]
            new_write_pos = n - first_part
        
        # Update positions atomically
        self._write_pos = new_write_pos % self.max_samples
        self._samples_available = min(self._samples_available + n, self.max_samples)
        
        # VAD processing for hands-free mode
        if self.vad and self._recording:
            # Process in VAD frames
            frame_size = self.vad.frame_samples
            for i in range(0, n - frame_size, frame_size):
                frame = (audio[i:i + frame_size] * 32767).astype(np.int16).tobytes()
                is_speech, endpoint = self.vad.process_frame(frame)
                
                if endpoint and self.on_voice_end:
                    self.on_voice_end()
                    break
            
            # Check for voice start
            if not self.vad.is_speaking and self.on_voice_start:
                # Check if we should start recording
                if self._samples_available > frame_size:
                    # Get latest frame
                    latest_frame = self._get_latest_samples(frame_size)
                    frame_bytes = (latest_frame * 32767).astype(np.int16).tobytes()
                    if self.vad.vad.is_speech(frame_bytes, 16000):
                        self.on_voice_start()
        
        # Track performance
        self._callback_count += 1
    
    def _get_latest_samples(self, n: int) -> np.ndarray:
        """Get the latest n samples from ring buffer (lock-free read)"""
        if n > self._samples_available:
            n = self._samples_available
        
        read_start = (self._write_pos - n) % self.max_samples
        
        if read_start + n <= self.max_samples:
            return self._ring_buffer[read_start:read_start + n].copy()
        else:
            first_part = self.max_samples - read_start
            return np.concatenate([
                self._ring_buffer[read_start:],
                self._ring_buffer[:n - first_part]
            ])
    
    def start(self, hands_free: bool = False):
        """
        Start recording
        
        Args:
            hands_free: Enable hands-free mode with VAD
        """
        if self._active:
            return
        
        # Reset state
        self._active = True
        self._recording = not hands_free  # In hands-free, wait for voice
        self._write_pos = 0
        self._read_pos = 0
        self._samples_available = 0
        self._ring_buffer.fill(0)
        
        if self.vad:
            self.vad.reset()
        
        # Start stream with optimal latency settings
        self._stream = sd.InputStream(
            callback=self._audio_callback,
            blocksize=self.buffer_samples,
            samplerate=self.samplerate,
            channels=self.channels,
            dtype='float32',
            latency='low'  # Request low latency
        )
        self._stream.start()
        
        if not hands_free:
            print("🎤 Recording...", file=sys.stderr)
    
    def stop(self) -> Optional[np.ndarray]:
        """
        Stop recording and return audio
        
        Returns:
            Recorded audio as numpy array, or None if no audio
        """
        if not self._active:
            return None
        
        self._active = False
        self._recording = False
        
        if self._stream:
            self._stream.stop()
            self._stream.close()
            self._stream = None
        
        # Extract all recorded audio from ring buffer
        if self._samples_available == 0:
            return None
        
        # Read all available samples
        n = self._samples_available
        if n == self.max_samples:
            # Buffer was full, read from write position
            audio = np.roll(self._ring_buffer, -self._write_pos)[:n]
        else:
            # Buffer not full, read from beginning
            if self._write_pos >= n:
                audio = self._ring_buffer[self._write_pos - n:self._write_pos]
            else:
                # Wrapped
                audio = np.concatenate([
                    self._ring_buffer[self.max_samples - (n - self._write_pos):],
                    self._ring_buffer[:self._write_pos]
                ])
        
        # Apply VAD filtering if enabled
        if self.vad and self.use_vad:
            filtered_audio, _ = self.vad.process_audio(audio, self.samplerate)
            if filtered_audio.size > 0:
                return filtered_audio
            else:
                print("(no speech detected)", file=sys.stderr)
                return None
        
        return audio
    
    def start_recording(self):
        """Start actual recording (for hands-free mode)"""
        self._recording = True
        if self.vad:
            self.vad.reset()
        print("🎤 Voice detected, recording...", file=sys.stderr)
    
    def is_active(self) -> bool:
        """Check if recorder is active"""
        return self._active
    
    def get_metrics(self) -> dict:
        """Get performance metrics"""
        return {
            'callbacks': self._callback_count,
            'samples_available': self._samples_available,
            'buffer_usage': self._samples_available / self.max_samples
        }