#!/usr/bin/env python3
"""
Voice Activity Detection using Silero VAD
Filters silence from audio to improve transcription speed and accuracy
"""
from __future__ import annotations
import numpy as np
import torch
from silero_vad import load_silero_vad, get_speech_timestamps

class SileroVAD:
    """
    Voice Activity Detection using Silero
    - Processes audio in 30ms frames
    - Filters out silence/noise
    - Returns only speech segments
    """
    
    def __init__(self, threshold: float = 0.5, min_speech_duration_ms: int = 250, min_silence_duration_ms: int = 100):
        """
        Initialize Silero VAD
        
        Args:
            threshold: Speech probability threshold (0.0-1.0)
            min_speech_duration_ms: Minimum speech segment duration to keep
            min_silence_duration_ms: Minimum silence duration to split segments
        """
        self.threshold = threshold
        self.min_speech_duration_ms = min_speech_duration_ms
        self.min_silence_duration_ms = min_silence_duration_ms
        
        # Load the Silero VAD model
        self.model = load_silero_vad(onnx=False)  # Use PyTorch backend for better macOS compatibility
        self.sample_rate = 16000  # Silero expects 16kHz
        
    def filter_audio(self, audio: np.ndarray, sample_rate: int = 16000) -> np.ndarray:
        """
        Filter audio to keep only speech segments
        
        Args:
            audio: Audio samples as numpy array
            sample_rate: Sample rate of the audio (must be 16000 for Silero)
            
        Returns:
            Filtered audio with only speech segments
        """
        if sample_rate != self.sample_rate:
            raise ValueError(f"Silero VAD requires {self.sample_rate}Hz audio, got {sample_rate}Hz")
        
        # Convert to torch tensor
        audio_tensor = torch.from_numpy(audio).float()
        
        # Get speech timestamps
        speech_timestamps = get_speech_timestamps(
            audio_tensor,
            self.model,
            threshold=self.threshold,
            min_speech_duration_ms=self.min_speech_duration_ms,
            min_silence_duration_ms=self.min_silence_duration_ms,
            return_seconds=False,  # Return sample indices
            sampling_rate=self.sample_rate
        )
        
        if not speech_timestamps:
            # No speech detected
            return np.array([], dtype=np.float32)
        
        # Collect speech segments
        speech_segments = []
        for segment in speech_timestamps:
            start = segment['start']
            end = segment['end']
            speech_segments.append(audio[start:end])
        
        # Concatenate all speech segments
        if speech_segments:
            return np.concatenate(speech_segments)
        else:
            return np.array([], dtype=np.float32)
    
    def get_speech_prob(self, audio_chunk: np.ndarray) -> float:
        """
        Get speech probability for a single audio chunk
        
        Args:
            audio_chunk: Audio chunk (should be ~30ms at 16kHz = 480 samples)
            
        Returns:
            Speech probability (0.0-1.0)
        """
        audio_tensor = torch.from_numpy(audio_chunk).float()
        
        # Ensure correct shape
        if audio_tensor.dim() == 1:
            audio_tensor = audio_tensor.unsqueeze(0)
        
        with torch.no_grad():
            speech_prob = self.model(audio_tensor, self.sample_rate).item()
        
        return speech_prob
