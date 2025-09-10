#!/usr/bin/env python3
"""
Unified transcription interface supporting multiple engines
"""
from __future__ import annotations
import sys
import os
import time
import orjson
from typing import Literal, Dict, Optional, Union
from .transcribe import Transcriber as WhisperTranscriber
from .transcribe_parakeet import ParakeetCoreMLTranscriber

OutputMode = Literal["text", "lower", "json"]
EngineType = Literal["whisper", "parakeet", "auto"]

class UnifiedTranscriber:
    """
    Unified transcriber supporting multiple engines
    - Whisper: Accurate, supports many languages
    - Parakeet: Fast (~5x real-time), English only
    - Auto: Choose based on availability and performance
    """
    
    def __init__(
        self, 
        engine: EngineType = "auto",
        model_name: str = "medium.en", 
        lang: str = "auto",
        idle_timeout_seconds: int = 60,
        custom_words: Optional[Dict[str, str]] = None
    ):
        self.engine_type = engine
        self.model_name = model_name
        self.lang = lang
        self.idle_timeout_seconds = idle_timeout_seconds
        self.custom_words = custom_words or {}
        
        self.whisper = None
        self.parakeet = None
        
        # Initialize the appropriate engine(s)
        self._initialize_engines()
    
    def _initialize_engines(self):
        """Initialize transcription engines based on selection"""
        if self.engine_type == "whisper":
            self._init_whisper()
        elif self.engine_type == "parakeet":
            try:
                self._init_parakeet()
            except Exception as e:
                print(f"⚠️ Parakeet initialization failed: {e}", file=sys.stderr)
                print(f"📝 Falling back to Whisper", file=sys.stderr)
                self._init_whisper()
        else:  # auto
            # Try Parakeet first for speed, fall back to Whisper
            try:
                self._init_parakeet()
                print("🚀 Using Parakeet CoreML for fast transcription", file=sys.stderr)
            except Exception as e:
                print(f"⚠️ Parakeet not available: {e}", file=sys.stderr)
                self._init_whisper()
                print("📝 Using Whisper for transcription", file=sys.stderr)
    
    def _init_whisper(self):
        """Initialize Whisper transcriber"""
        self.whisper = WhisperTranscriber(
            model_name=self.model_name,
            lang=self.lang,
            idle_timeout_seconds=self.idle_timeout_seconds,
            custom_words=self.custom_words
        )
    
    def _init_parakeet(self):
        """Initialize Parakeet transcriber"""
        self.parakeet = ParakeetCoreMLTranscriber(custom_words=self.custom_words)
        if not self.parakeet.is_available():
            raise RuntimeError("Parakeet CLI not available")
    
    def transcribe(self, audio_f32_mono_16k, output: OutputMode = "text"):
        """
        Transcribe audio using the selected engine
        
        Args:
            audio_f32_mono_16k: Audio samples (float32, mono, 16kHz)
            output: Output format (text, lower, json)
            
        Returns:
            Transcribed text or None if no speech detected
        """
        text = None
        
        # Try Parakeet first if available
        if self.parakeet:
            try:
                text = self.parakeet.transcribe(audio_f32_mono_16k, sample_rate=16000)
                if text:
                    print(f"✅ Transcribed with Parakeet", file=sys.stderr)
            except Exception as e:
                print(f"⚠️ Parakeet failed, falling back: {e}", file=sys.stderr)
                text = None
        
        # Fall back to Whisper if Parakeet didn't work
        if text is None and self.whisper:
            text = self.whisper.transcribe(audio_f32_mono_16k, output=output)
            if text and output == "text":  # Don't double-print for other formats
                return text  # Whisper already handles output formatting
        
        if not text:
            print("(no speech detected)", file=sys.stderr)
            return None
        
        # Process output based on mode (for Parakeet results)
        if output == "lower":
            text = text.lower()
        elif output == "json":
            payload = {
                "text": text,
                "engine": "parakeet" if self.parakeet else "whisper"
            }
            text = orjson.dumps(payload).decode("utf-8")
        
        # Print for debugging/piping
        print(text, flush=True)
        
        return text
    
    def get_active_engine(self) -> str:
        """Get the name of the active transcription engine"""
        if self.parakeet:
            return "parakeet"
        elif self.whisper:
            return "whisper"
        else:
            return "none"