#!/usr/bin/env python3
"""
WebRTC VAD for ultra-low latency voice activity detection
- 20ms frame processing
- Configurable aggressiveness
- Hangover/smoothing for robust endpoint detection
"""
from __future__ import annotations
import numpy as np
import webrtcvad
from typing import Optional, Tuple
import collections

class WebRTCVAD:
    """
    WebRTC Voice Activity Detection
    - Processes audio in 10/20/30ms frames
    - Low latency (< 5ms per frame)
    - Configurable aggressiveness and hangover
    """
    
    def __init__(self,
                 aggressiveness: int = 2,
                 frame_duration_ms: int = 20,
                 hangover_ms: int = 300,
                 min_utterance_ms: int = 300,
                 max_utterance_ms: int = 10000):
        """
        Initialize WebRTC VAD
        
        Args:
            aggressiveness: 0-3, higher = more aggressive filtering
            frame_duration_ms: Frame size (10, 20, or 30ms)
            hangover_ms: Time to wait after speech ends
            min_utterance_ms: Minimum utterance duration
            max_utterance_ms: Maximum utterance duration
        """
        self.vad = webrtcvad.Vad(aggressiveness)
        self.frame_duration_ms = frame_duration_ms
        self.hangover_ms = hangover_ms
        self.min_utterance_ms = min_utterance_ms
        self.max_utterance_ms = max_utterance_ms
        
        # Frame size in samples (16kHz)
        self.frame_samples = int(16000 * frame_duration_ms / 1000)
        
        # Hangover in frames
        self.hangover_frames = int(hangover_ms / frame_duration_ms)
        
        # State tracking
        self.reset()
    
    def reset(self):
        """Reset VAD state"""
        self.speech_frames = []
        self.silence_frames = 0
        self.is_speaking = False
        self.utterance_start_time = None
        self.total_frames = 0
        
        # Ring buffer for smoothing
        self.frame_buffer = collections.deque(maxlen=5)
    
    def process_frame(self, frame: bytes) -> Tuple[bool, bool]:
        """
        Process a single audio frame
        
        Args:
            frame: Audio frame as bytes (16-bit PCM, 16kHz)
            
        Returns:
            (is_speech, endpoint_detected)
        """
        # Detect speech in this frame
        is_speech = self.vad.is_speech(frame, 16000)
        
        # Add to smoothing buffer
        self.frame_buffer.append(is_speech)
        
        # Smooth decision (majority vote)
        smoothed_speech = sum(self.frame_buffer) >= len(self.frame_buffer) // 2
        
        self.total_frames += 1
        endpoint_detected = False
        
        if smoothed_speech:
            # Speech detected
            self.silence_frames = 0
            
            if not self.is_speaking:
                # Start of utterance
                self.is_speaking = True
                self.utterance_start_time = self.total_frames
                self.speech_frames = []
            
            # Check max utterance length
            utterance_duration_ms = (self.total_frames - self.utterance_start_time) * self.frame_duration_ms
            if utterance_duration_ms >= self.max_utterance_ms:
                endpoint_detected = True
        else:
            # Silence detected
            if self.is_speaking:
                self.silence_frames += 1
                
                # Check if we've hit the hangover threshold
                if self.silence_frames >= self.hangover_frames:
                    # Check minimum utterance length
                    utterance_duration_ms = (self.total_frames - self.utterance_start_time) * self.frame_duration_ms
                    if utterance_duration_ms >= self.min_utterance_ms:
                        endpoint_detected = True
                    else:
                        # Too short, reset
                        self.is_speaking = False
                        self.silence_frames = 0
        
        return smoothed_speech, endpoint_detected
    
    def process_audio(self, audio: np.ndarray, sample_rate: int = 16000) -> Tuple[np.ndarray, bool]:
        """
        Process audio and detect endpoint
        
        Args:
            audio: Audio samples as numpy array
            sample_rate: Sample rate (must be 16000)
            
        Returns:
            (filtered_audio, endpoint_detected)
        """
        if sample_rate != 16000:
            raise ValueError("WebRTC VAD requires 16kHz audio")
        
        # Convert to 16-bit PCM bytes
        audio_int16 = (audio * 32767).astype(np.int16)
        
        speech_segments = []
        current_segment = []
        endpoint_detected = False
        
        # Process in frames
        for i in range(0, len(audio_int16) - self.frame_samples, self.frame_samples):
            frame = audio_int16[i:i + self.frame_samples].tobytes()
            is_speech, is_endpoint = self.process_frame(frame)
            
            if is_speech or (self.is_speaking and self.silence_frames < self.hangover_frames):
                # Include this frame (speech or within hangover)
                current_segment.append(audio[i:i + self.frame_samples])
            elif current_segment:
                # End of speech segment
                speech_segments.append(np.concatenate(current_segment))
                current_segment = []
            
            if is_endpoint:
                endpoint_detected = True
                break
        
        # Handle remaining segment
        if current_segment:
            speech_segments.append(np.concatenate(current_segment))
        
        # Combine all speech segments
        if speech_segments:
            filtered_audio = np.concatenate(speech_segments)
        else:
            filtered_audio = np.array([], dtype=np.float32)
        
        return filtered_audio, endpoint_detected
    
    def is_endpoint(self) -> bool:
        """Check if endpoint has been detected"""
        if not self.is_speaking:
            return False
        
        # Check hangover
        if self.silence_frames >= self.hangover_frames:
            utterance_duration_ms = (self.total_frames - self.utterance_start_time) * self.frame_duration_ms
            return utterance_duration_ms >= self.min_utterance_ms
        
        # Check max duration
        if self.utterance_start_time is not None:
            utterance_duration_ms = (self.total_frames - self.utterance_start_time) * self.frame_duration_ms
            return utterance_duration_ms >= self.max_utterance_ms
        
        return False