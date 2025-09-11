#!/usr/bin/env python3
"""
Unified transcription interface
"""
from __future__ import annotations
import sys
import os
import time
import orjson
from typing import Literal, Dict, Optional, Union
from .transcribe import Transcriber as WhisperTranscriber

OutputMode = Literal["text", "lower", "json"]
EngineType = Literal["whisper", "auto"]

class UnifiedTranscriber:
    """
    Unified transcriber using Whisper
    - Whisper: Accurate, supports many languages
    - Auto: Same as whisper for now
    """
    
    def __init__(
        self, 
        engine: EngineType = "whisper",
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
        
        # Initialize the engine
        self._initialize_engines()
    
    def _initialize_engines(self):
        """Initialize transcription engine"""
        self._init_whisper()
    
    def _init_whisper(self):
        """Initialize Whisper transcriber"""
        self.whisper = WhisperTranscriber(
            model_name=self.model_name,
            lang=self.lang,
            idle_timeout_seconds=self.idle_timeout_seconds,
            custom_words=self.custom_words
        )
    
    def transcribe(self, audio_f32_mono_16k, output: OutputMode = "text"):
        """
        Transcribe audio using Whisper
        
        Args:
            audio_f32_mono_16k: Audio samples (float32, mono, 16kHz)
            output: Output format (text, lower, json)
            
        Returns:
            Transcribed text or None if no speech detected
        """
        if self.whisper:
            text = self.whisper.transcribe(audio_f32_mono_16k, output=output)
            return text
        
        print("(no transcriber available)", file=sys.stderr)
        return None
    
    def get_active_engine(self) -> str:
        """Get the name of the active transcription engine"""
        if self.whisper:
            return "whisper"
        else:
            return "none"