#!/usr/bin/env python3
"""
Optimized Transcription
- Minimal stdout/stderr redirection overhead
- Efficient output suppression
- Fast processing pipeline
"""
from __future__ import annotations
import sys
import os
import orjson
from typing import Literal
from pywhispercpp.model import Model

OutputMode = Literal["text", "lower", "json"]

class Transcriber:
    """
    Optimized Transcriber
    - Minimal overhead during transcription
    - Efficient output suppression
    """
    def __init__(self, model_name: str, lang: str = "auto"):
        print(f"Loading Whisper model: {model_name}", file=sys.stderr)
        
        # Suppress output during model initialization
        old_stdout = os.dup(1)
        old_stderr = os.dup(2)
        with open(os.devnull, 'w') as devnull:
            os.dup2(devnull.fileno(), 1)
            os.dup2(devnull.fileno(), 2)
            try:
                self.model = Model(model_name, language=lang)
            finally:
                os.dup2(old_stdout, 1)
                os.dup2(old_stderr, 2)
                os.close(old_stdout)
                os.close(old_stderr)

    def transcribe(self, audio_f32_mono_16k, output: OutputMode = "text"):
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