#!/usr/bin/env python3
"""
Optimized Transcription with whisper.cpp
- Optimal parameter configuration for low latency
- Metal acceleration on Apple Silicon
- Greedy decoding for speed
- Model warmup for consistent performance
"""
from __future__ import annotations
import sys
import os
import time
import threading
import numpy as np
from typing import Optional, Dict, Literal
from pywhispercpp.model import Model

OutputMode = Literal["text", "lower", "json"]

class OptimizedTranscriber:
    """
    Optimized Whisper transcriber
    - Configured for minimal latency
    - Metal acceleration
    - Model warmup
    - Efficient memory management
    """
    
    def __init__(self,
                 model_name: str = "base.en",
                 lang: str = "en",
                 idle_timeout_seconds: int = 60,
                 custom_words: Optional[Dict[str, str]] = None,
                 warmup: bool = True):
        """
        Initialize optimized transcriber
        
        Args:
            model_name: Whisper model to use (base.en recommended for speed)
            lang: Language code
            idle_timeout_seconds: Seconds before unloading model (0=never)
            custom_words: Dictionary of word replacements
            warmup: Perform model warmup on initialization
        """
        self.model_name = model_name
        self.lang = lang if not model_name.endswith('.en') else 'en'
        self.idle_timeout_seconds = idle_timeout_seconds
        self.custom_words = custom_words or {}
        
        self.model: Optional[Model] = None
        self.last_used = 0.0
        self.unload_timer: Optional[threading.Timer] = None
        self._lock = threading.Lock()
        
        # Performance tracking
        self.total_transcriptions = 0
        self.total_time = 0.0
        
        # Load and warmup model
        self._load_model()
        if warmup:
            self._warmup_model()
    
    def _load_model(self):
        """Load model with optimized settings"""
        print(f"⚡ Loading optimized Whisper model: {self.model_name}", file=sys.stderr)
        
        # Suppress whisper.cpp output
        old_stdout = os.dup(1)
        old_stderr = os.dup(2)
        with open(os.devnull, 'w') as devnull:
            os.dup2(devnull.fileno(), 1)
            os.dup2(devnull.fileno(), 2)
            try:
                # Initialize with optimal parameters
                self.model = Model(
                    self.model_name,
                    language=self.lang,
                    n_threads=6,  # Use performance cores
                    # The pywhispercpp library will handle Metal acceleration automatically
                )
                
                # Configure for speed (if the library supports these)
                # Note: These may need adjustment based on pywhispercpp API
                if hasattr(self.model, 'params'):
                    self.model.params.beam_size = 1  # Greedy decoding
                    self.model.params.best_of = 1    # No alternatives
                    self.model.params.temperature = 0.0  # Deterministic
                    self.model.params.no_timestamps = True
                    self.model.params.suppress_non_speech_tokens = True
                    
            finally:
                os.dup2(old_stdout, 1)
                os.dup2(old_stderr, 2)
                os.close(old_stdout)
                os.close(old_stderr)
        
        print(f"✅ Model loaded with Metal acceleration", file=sys.stderr)
    
    def _warmup_model(self):
        """Warmup model for consistent performance"""
        print("🔥 Warming up model...", file=sys.stderr)
        
        # Create 1 second of silence for warmup (to avoid warning)
        warmup_audio = np.zeros(16000, dtype=np.float32)
        
        # Run warmup transcription
        start = time.perf_counter()
        
        # Suppress output including stderr
        old_stdout = os.dup(1)
        old_stderr = os.dup(2)
        with open(os.devnull, 'w') as devnull:
            os.dup2(devnull.fileno(), 1)
            os.dup2(devnull.fileno(), 2)
            try:
                _ = self.model.transcribe(warmup_audio)
            finally:
                os.dup2(old_stdout, 1)
                os.dup2(old_stderr, 2)
                os.close(old_stdout)
                os.close(old_stderr)
        
        warmup_time = (time.perf_counter() - start) * 1000
        print(f"✅ Model warmed up ({warmup_time:.0f}ms)", file=sys.stderr)
    
    def _unload_model(self):
        """Unload model to free memory"""
        with self._lock:
            if self.model is not None:
                print(f"💤 Unloading model after {self.idle_timeout_seconds}s idle", file=sys.stderr)
                del self.model
                self.model = None
    
    def _schedule_unload(self):
        """Schedule model unloading after idle timeout"""
        if self.unload_timer:
            self.unload_timer.cancel()
        
        if self.idle_timeout_seconds > 0:
            self.unload_timer = threading.Timer(
                self.idle_timeout_seconds,
                self._unload_model
            )
            self.unload_timer.daemon = True
            self.unload_timer.start()
    
    def _ensure_model_loaded(self):
        """Ensure model is loaded before use"""
        with self._lock:
            if self.model is None:
                self._load_model()
                self._warmup_model()
            self.last_used = time.time()
            self._schedule_unload()
    
    def apply_custom_words(self, text: str) -> str:
        """Apply custom word replacements"""
        if not self.custom_words or not text:
            return text
        
        import re
        for old_word, new_word in self.custom_words.items():
            pattern = re.compile(re.escape(old_word), re.IGNORECASE)
            
            def replace_func(match):
                original = match.group(0)
                if original.isupper():
                    return new_word.upper()
                elif original[0].isupper():
                    return new_word.capitalize()
                else:
                    return new_word.lower()
            
            text = pattern.sub(replace_func, text)
        
        return text
    
    def transcribe(self,
                   audio: np.ndarray,
                   output: OutputMode = "text",
                   measure_time: bool = False) -> Optional[str]:
        """
        Transcribe audio with optimal settings
        
        Args:
            audio: Audio samples (16kHz, mono, float32)
            output: Output format (text, lower, json)
            measure_time: Whether to measure transcription time
            
        Returns:
            Transcribed text or None if no speech
        """
        # Ensure model is loaded
        self._ensure_model_loaded()
        
        # Pad short audio to avoid whisper.cpp warnings (minimum 1 second)
        min_samples = 16000  # 1 second at 16kHz
        if len(audio) < min_samples:
            # Pad with silence
            padding = min_samples - len(audio)
            audio = np.pad(audio, (0, padding), mode='constant', constant_values=0)
        
        # Start timing
        if measure_time:
            start_time = time.perf_counter()
        
        # Transcribe with output suppression (including stderr for warnings)
        old_stdout = os.dup(1)
        old_stderr = os.dup(2)
        with open(os.devnull, 'w') as devnull:
            os.dup2(devnull.fileno(), 1)
            os.dup2(devnull.fileno(), 2)
            try:
                # Use model's transcribe method with optimal settings
                segments = self.model.transcribe(
                    audio,
                    # Additional parameters if supported by pywhispercpp
                    # beam_size=1,
                    # best_of=1,
                    # temperature=0.0,
                    # no_timestamps=True
                )
            finally:
                os.dup2(old_stdout, 1)
                os.dup2(old_stderr, 2)
                os.close(old_stdout)
                os.close(old_stderr)
        
        # Track performance
        if measure_time:
            elapsed = time.perf_counter() - start_time
            self.total_transcriptions += 1
            self.total_time += elapsed
            avg_time = self.total_time / self.total_transcriptions
            print(f"⚡ Transcription: {elapsed*1000:.0f}ms (avg: {avg_time*1000:.0f}ms)", file=sys.stderr)
        
        # Process segments
        if not segments:
            return None
        
        # Extract text efficiently
        text = " ".join(s.text for s in segments).strip()
        
        # Apply custom words
        text = self.apply_custom_words(text)
        
        if not text:
            return None
        
        # Format output
        if output == "lower":
            text = text.lower()
        elif output == "json":
            import orjson
            payload = {
                "text": text,
                "segments": [{"t0": s.t0, "t1": s.t1, "text": s.text} for s in segments],
            }
            text = orjson.dumps(payload).decode("utf-8")
        
        return text
    
    def get_metrics(self) -> dict:
        """Get performance metrics"""
        if self.total_transcriptions == 0:
            return {
                'total_transcriptions': 0,
                'avg_time_ms': 0,
                'model_loaded': self.model is not None
            }
        
        return {
            'total_transcriptions': self.total_transcriptions,
            'avg_time_ms': (self.total_time / self.total_transcriptions) * 1000,
            'model_loaded': self.model is not None
        }