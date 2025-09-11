#!/usr/bin/env python3
"""
Optimized local dictation with improved performance
"""
from __future__ import annotations
import argparse
import os
import sys
import time
import json
from pynput import keyboard
from .hotkey import HotkeyListener, parse_chord
from .audio import VoiceRecorder, list_input_devices
from .transcribe import Transcriber
from .transcribe_unified import UnifiedTranscriber
from .assistant import Assistant
from .config import get_config_path, load_config

def env_or(name: str, default: str):
    return os.getenv(name, default)

def build_argparser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser("local-dictation", 
                               description="Fast push-to-talk dictation (macOS, Apple Silicon)")
    p.add_argument("--model", default=env_or("MODEL", "medium.en"),
                   help="Whisper model (tiny.en, base.en, small.en, medium.en, large-v3-turbo-q8_0)")
    p.add_argument("--lang", default=env_or("LANG", "en"),
                   help="Language code (en, auto, etc.)")
    p.add_argument("--chord", default=env_or("CHORD", "CMD,ALT"),
                   help="Chord like 'CMD,ALT' or 'CTRL,SHIFT' or 'F8'")
    p.add_argument("--debounce-ms", type=int, default=int(env_or("DEBOUNCE_MS", "50")),
                   help="Key release debounce in milliseconds")
    p.add_argument("--max-sec", type=float, default=float(env_or("MAX_SEC", "90")))
    p.add_argument("--highpass-hz", type=float, default=float(env_or("HIGHPASS_HZ", "0")))
    p.add_argument("--device", default=env_or("AUDIO_DEVICE", None),
                   help="Substring match of input device name")
    p.add_argument("--output", choices=["text","lower","json"], default=env_or("OUTPUT","text"))
    p.add_argument("--print-devices", action="store_true")
    p.add_argument("--benchmark", action="store_true", 
                   help="Show performance timing for each transcription")
    p.add_argument("--assistant-mode", action="store_true",
                   help="Enable assistant mode for processing commands on selected text")
    p.add_argument("--assistant-model", default=env_or("ASSISTANT_MODEL", "mlx-community/Llama-3.2-3B-Instruct-4bit"),
                   help="MLX model to use for assistant mode")
    p.add_argument("--use-vad", action="store_true",
                   help="Enable VAD (Voice Activity Detection) to filter silence")
    p.add_argument("--idle-timeout", type=int, default=int(env_or("IDLE_TIMEOUT", "60")),
                   help="Seconds before unloading idle model (0=never)")
    p.add_argument("--custom-words", type=str, default=env_or("CUSTOM_WORDS", None),
                   help="JSON file with custom word replacements")
    p.add_argument("--engine", choices=["whisper", "auto"], default=env_or("ENGINE", "whisper"),
                   help="Transcription engine")
    return p

def main():
    args = build_argparser().parse_args()
    
    # Load config (creates default if doesn't exist)
    config_path = get_config_path()
    if not config_path.exists():
        print(f"🔧 Creating config file at: {config_path}", file=sys.stderr)
        config = load_config()  # This will create default config
        print(f"📝 Please edit the config to set your email sign-off", file=sys.stderr)
    else:
        config = load_config()

    if args.print_devices:
        devs = list_input_devices()
        if not devs:
            print("No input devices.", file=sys.stderr)
            sys.exit(1)
        for d in devs:
            print(f"[{d['index']:02d}] {d['name']}  (default_sr={d['default_samplerate']})")
        return

    chord = parse_chord(args.chord)
    if not chord:
        print(f"Invalid chord: {args.chord}", file=sys.stderr)
        sys.exit(2)

    rec = VoiceRecorder(device_name=args.device,
                        max_sec=args.max_sec,
                        highpass_hz=args.highpass_hz,
                        channels=1,
                        use_vad=args.use_vad)

    # Load custom words if provided
    custom_words = {}
    if args.custom_words:
        try:
            with open(args.custom_words, 'r') as f:
                custom_words = json.load(f)
            print(f"📖 Loaded {len(custom_words)} custom word replacements", file=sys.stderr)
        except Exception as e:
            print(f"⚠️ Failed to load custom words: {e}", file=sys.stderr)

    # Force English language for .en models
    lang = 'en' if args.model.endswith('.en') else args.lang
    
    # Use unified transcriber for engine selection
    tx = UnifiedTranscriber(
        engine=args.engine,
        model_name=args.model, 
        lang=lang, 
        idle_timeout_seconds=args.idle_timeout,
        custom_words=custom_words
    )
    
    # Initialize assistant if in assistant mode
    assistant = None
    if args.assistant_mode:
        print(f"🤖 Initializing Assistant Mode...", file=sys.stderr)
        assistant = Assistant(model_name=args.assistant_model)
        assistant.enable()
        if assistant.enabled:
            print(f"🤖 Assistant Mode: ON (model: {args.assistant_model})", file=sys.stderr)
            print(f"   Commands: 'rewrite this...', 'explain this', 'translate to...', etc.", file=sys.stderr)
        else:
            print(f"⚠️  Assistant Mode failed to initialize. Running in dictation-only mode.", file=sys.stderr)
            assistant = None

    print(f"🎤 Local Dictation", file=sys.stderr)
    print(f"Engine: {tx.get_active_engine()}", file=sys.stderr)
    if tx.get_active_engine() == "whisper":
        print(f"Model: {args.model}", file=sys.stderr)
    print(f"Press and hold {args.chord} to record", file=sys.stderr)
    print(f"Debounce: {args.debounce_ms}ms", file=sys.stderr)
    if args.use_vad:
        print(f"🔇 VAD: Enabled (silence filtering)", file=sys.stderr)
    if args.idle_timeout > 0 and tx.get_active_engine() == "whisper":
        print(f"💤 Model unload after: {args.idle_timeout}s idle", file=sys.stderr)
    
    if rec.needs_resample:
        print(f"⚠️  Device rate: {rec.samplerate}Hz (will resample to 16kHz)", file=sys.stderr)
    else:
        print(f"✅ Direct 16kHz recording (no resampling needed)", file=sys.stderr)

    # Create a keyboard controller for typing
    kbd = keyboard.Controller()
    
    # Performance tracking
    timings = []
    recording_start = None
    
    def on_chord(active: bool):
        nonlocal recording_start
        try:
            if active:
                recording_start = time.perf_counter()
                rec.start()
            else:
                recording_end = time.perf_counter()
                recording_duration = recording_end - recording_start
                
                # Measure audio processing
                audio_start = time.perf_counter()
                audio = rec.stop()
                audio_end = time.perf_counter()
                audio_process_time = audio_end - audio_start
                
                if audio is not None and audio.size > 0:
                    # Measure transcription
                    transcribe_start = time.perf_counter()
                    text = tx.transcribe(audio, output=args.output)
                    transcribe_end = time.perf_counter()
                    transcribe_time = transcribe_end - transcribe_start
                    
                    if args.benchmark and text:
                        # Calculate true processing time (excluding recording duration)
                        processing_time = audio_process_time + transcribe_time
                        total_time = recording_duration + processing_time
                        
                        timings.append({
                            'recording': recording_duration,
                            'audio_processing': audio_process_time,
                            'transcription': transcribe_time,
                            'processing_total': processing_time,
                            'total': total_time
                        })
                        
                        print(f"\n⏱️  Timing breakdown:", file=sys.stderr)
                        print(f"  Recording duration: {recording_duration*1000:.0f}ms", file=sys.stderr)
                        print(f"  Audio processing: {audio_process_time*1000:.0f}ms", file=sys.stderr)
                        print(f"  Transcription: {transcribe_time*1000:.0f}ms", file=sys.stderr)
                        print(f"  → Processing time: {processing_time*1000:.0f}ms (what matters!)", file=sys.stderr)
                        
                        if len(timings) >= 3:
                            avg_processing = sum(t['processing_total'] for t in timings[-5:]) / min(5, len(timings))
                            print(f"📊 Avg processing (last 5): {avg_processing*1000:.0f}ms", file=sys.stderr)
                    
                    if text:
                        # In assistant mode, try to process as command first
                        if assistant and assistant.process_transcription(text):
                            # Command was processed, no need to type
                            if args.benchmark:
                                print(f"✅ Command processed", file=sys.stderr)
                        else:
                            # Apply app-aware formatting if assistant is enabled
                            if assistant and assistant.enabled:
                                text = assistant.format_for_app_context(text)  # Will use config for sign-off
                            
                            # Regular dictation - type the transcribed text
                            kbd.type(text)
        except Exception as e:
            print(f"Error: {e}", file=sys.stderr)

    listener = HotkeyListener(chord=chord, debounce_ms=args.debounce_ms, on_chord_active=on_chord)
    
    print("\n" + "="*60, file=sys.stderr)
    print("Ready! Hold chord to record, release to transcribe.", file=sys.stderr)
    if args.benchmark:
        print("Benchmark mode: Showing detailed timing breakdown", file=sys.stderr)
    print("="*60 + "\n", file=sys.stderr)
    
    try:
        listener.start()
        listener.join()
    except KeyboardInterrupt:
        listener.stop()
        print("\nStopped.", file=sys.stderr)
        
        if args.benchmark and timings:
            print("\n" + "="*60, file=sys.stderr)
            print("📈 SESSION SUMMARY", file=sys.stderr)
            print("="*60, file=sys.stderr)
            
            avg_processing = sum(t['processing_total'] for t in timings) / len(timings)
            min_processing = min(t['processing_total'] for t in timings)
            max_processing = max(t['processing_total'] for t in timings)
            
            avg_transcribe = sum(t['transcription'] for t in timings) / len(timings)
            avg_audio = sum(t['audio_processing'] for t in timings) / len(timings)
            
            print(f"Runs: {len(timings)}", file=sys.stderr)
            print(f"\nProcessing time (excluding recording):", file=sys.stderr)
            print(f"  Average: {avg_processing*1000:.0f}ms", file=sys.stderr)
            print(f"  Fastest: {min_processing*1000:.0f}ms", file=sys.stderr)
            print(f"  Slowest: {max_processing*1000:.0f}ms", file=sys.stderr)
            print(f"\nBreakdown averages:", file=sys.stderr)
            print(f"  Audio processing: {avg_audio*1000:.0f}ms", file=sys.stderr)
            print(f"  Transcription: {avg_transcribe*1000:.0f}ms", file=sys.stderr)
            
            print("\n💡 Performance tips:", file=sys.stderr)
            if args.model == "large-v3-turbo-q8_0":
                print("  • Try --model base.en for 3-5x faster transcription", file=sys.stderr)
            if rec.needs_resample:
                print("  • Audio resampling adds latency", file=sys.stderr)
            if args.debounce_ms > 30:
                print(f"  • Try --debounce-ms 30 to reduce delay by {args.debounce_ms-30}ms", file=sys.stderr)