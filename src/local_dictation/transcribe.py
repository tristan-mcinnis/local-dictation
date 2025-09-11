#!/usr/bin/env python3
"""
Optimized Transcription
- Minimal stdout/stderr redirection overhead
- Efficient output suppression
- Fast processing pipeline
- Model idle unloading
- Custom word dictionary
- Whisper engine support
"""
from __future__ import annotations
import sys
import os
import time
import threading
import orjson
from typing import Literal, Dict, Optional
from pywhispercpp.model import Model

OutputMode = Literal["text", "lower", "json"]

class Transcriber:
    """
    Optimized Transcriber
    - Minimal overhead during transcription
    - Efficient output suppression
    - Auto-unloads model after idle timeout
    - Custom word replacements
    """
    def __init__(self, model_name: str, lang: str = "auto", idle_timeout_seconds: int = 60, custom_words: Optional[Dict[str, str]] = None):
        self.model_name = model_name
        self.lang = lang
        self.idle_timeout_seconds = idle_timeout_seconds
        self.custom_words = custom_words or {}
        
        self.model: Optional[Model] = None
        self.last_used = 0.0
        self.unload_timer: Optional[threading.Timer] = None
        self._lock = threading.Lock()
        
        # Load model on first use
        self._ensure_model_loaded()
    
    def _load_model(self):
        """Load the model with output suppression"""
        print(f"Loading Whisper model: {self.model_name}", file=sys.stderr)
        
        # Suppress output during model initialization
        old_stdout = os.dup(1)
        old_stderr = os.dup(2)
        with open(os.devnull, 'w') as devnull:
            os.dup2(devnull.fileno(), 1)
            os.dup2(devnull.fileno(), 2)
            try:
                self.model = Model(self.model_name, language=self.lang)
            finally:
                os.dup2(old_stdout, 1)
                os.dup2(old_stderr, 2)
                os.close(old_stdout)
                os.close(old_stderr)
    
    def _unload_model(self):
        """Unload the model to free memory"""
        with self._lock:
            if self.model is not None:
                print(f"Unloading model after {self.idle_timeout_seconds}s idle", file=sys.stderr)
                del self.model
                self.model = None
    
    def _schedule_unload(self):
        """Schedule model unloading after idle timeout"""
        # Cancel existing timer if any
        if self.unload_timer:
            self.unload_timer.cancel()
        
        # Schedule new unload
        if self.idle_timeout_seconds > 0:
            self.unload_timer = threading.Timer(self.idle_timeout_seconds, self._unload_model)
            self.unload_timer.daemon = True
            self.unload_timer.start()
    
    def _ensure_model_loaded(self):
        """Ensure model is loaded before use"""
        with self._lock:
            if self.model is None:
                self._load_model()
            self.last_used = time.time()
            self._schedule_unload()
    
    def apply_custom_words(self, text: str) -> str:
        """Apply custom word replacements"""
        if not self.custom_words:
            return text
        
        for old_word, new_word in self.custom_words.items():
            # Case-insensitive replacement while preserving original case
            import re
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

    def transcribe(self, audio_f32_mono_16k, output: OutputMode = "text"):
        # Ensure model is loaded
        self._ensure_model_loaded()
        
        # Minimal suppression - only stdout during transcription
        old_stdout = os.dup(1)
        with open(os.devnull, 'w') as devnull:
            os.dup2(devnull.fileno(), 1)
            try:
                segments = self.model.transcribe(audio_f32_mono_16k)
            finally:
                os.dup2(old_stdout, 1)
                os.close(old_stdout)
        
        if not segments:
            print("(no speech detected)", file=sys.stderr)
            return None
        
        # Extract and process text efficiently
        text = " ".join(s.text for s in segments).strip()
        
        # Apply custom word replacements
        text = self.apply_custom_words(text)
        
        if not text:
            print("(no speech detected)", file=sys.stderr)
            return None

        # Process output based on mode
        if output == "lower":
            text = text.lower()
        elif output == "json":
            payload = {
                "text": text,
                "segments": [{"t0": s.t0, "t1": s.t1, "text": s.text} for s in segments],
            }
            text = orjson.dumps(payload).decode("utf-8")
        
        # Print for debugging/piping
        print(text, flush=True)
        
        # Return for typing
        return text