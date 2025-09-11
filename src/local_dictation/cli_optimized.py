#!/usr/bin/env python3
"""
Optimized Local Dictation CLI
- Ultra-low latency (<500ms end-to-end)
- Hands-free mode with double-tap activation
- WebRTC VAD for voice activity detection
- Direct text insertion via Accessibility API
- Comprehensive performance metrics
"""
from __future__ import annotations
import argparse
import os
import sys
import time
import json
from pathlib import Path

from .hotkey_enhanced import EnhancedHotkeyListener, parse_chord, RecordingMode
from .audio_optimized import OptimizedVoiceRecorder, list_input_devices
from .transcribe_optimized import OptimizedTranscriber
from .text_insert import TextInserter
from .metrics import PerformanceTracker
from .assistant import Assistant
from .config import get_config_path, load_config

def env_or(name: str, default: str):
    """Get environment variable or default"""
    return os.getenv(name, default)

def build_argparser() -> argparse.ArgumentParser:
    """Build argument parser with all options"""
    p = argparse.ArgumentParser(
        "local-dictation-optimized",
        description="Ultra-low latency push-to-talk dictation for macOS"
    )
    
    # Model configuration
    p.add_argument("--model", default=env_or("MODEL", "base.en"),
                   help="Whisper model (tiny.en/base.en for speed, medium.en for accuracy)")
    p.add_argument("--lang", default=env_or("LANG", "en"),
                   help="Language code (en, auto, etc.)")
    
    # Hotkey configuration
    p.add_argument("--chord", default=env_or("CHORD", "CMD,ALT"),
                   help="Hotkey chord (e.g., 'CMD,ALT' or 'CTRL,SHIFT')")
    p.add_argument("--debounce-ms", type=int, default=int(env_or("DEBOUNCE_MS", "30")),
                   help="Key release debounce in milliseconds")
    p.add_argument("--double-tap-ms", type=int, default=int(env_or("DOUBLE_TAP_MS", "500")),
                   help="Max time between taps for double-tap detection")
    
    # Audio configuration
    p.add_argument("--device", default=env_or("AUDIO_DEVICE", None),
                   help="Audio input device name (substring match)")
    p.add_argument("--max-sec", type=float, default=float(env_or("MAX_SEC", "90")),
                   help="Maximum recording duration in seconds")
    p.add_argument("--buffer-ms", type=int, default=int(env_or("BUFFER_MS", "20")),
                   help="Audio buffer size in milliseconds (10-20ms optimal)")
    
    # VAD configuration
    p.add_argument("--use-vad", action="store_true",
                   help="Enable VAD for hands-free mode")
    p.add_argument("--vad-aggressiveness", type=int, default=2, choices=[0,1,2,3],
                   help="VAD aggressiveness (0-3, higher = more aggressive)")
    p.add_argument("--vad-frame-ms", type=int, default=20, choices=[10,20,30],
                   help="VAD frame duration in milliseconds")
    p.add_argument("--vad-hangover-ms", type=int, default=300,
                   help="VAD hangover time in milliseconds")
    p.add_argument("--vad-min-utterance-ms", type=int, default=300,
                   help="Minimum utterance duration in milliseconds")
    p.add_argument("--vad-max-utterance-ms", type=int, default=10000,
                   help="Maximum utterance duration in milliseconds")
    
    # Text insertion
    p.add_argument("--use-ax-api", action="store_true", default=True,
                   help="Use Accessibility API for text insertion")
    p.add_argument("--paste-delay-ms", type=int, default=10,
                   help="Delay after clipboard write before paste")
    
    # Output options
    p.add_argument("--output", choices=["text","lower","json"], default="text",
                   help="Output format")
    p.add_argument("--custom-words", type=str, default=None,
                   help="JSON file with custom word replacements")
    
    # Assistant mode
    p.add_argument("--assistant-mode", action="store_true",
                   help="Enable assistant mode for text processing")
    p.add_argument("--assistant-model", 
                   default=env_or("ASSISTANT_MODEL", "mlx-community/Llama-3.2-3B-Instruct-4bit"),
                   help="MLX model for assistant mode")
    
    # Performance options
    p.add_argument("--warmup", action="store_true", default=True,
                   help="Warmup model on startup for consistent performance")
    p.add_argument("--idle-timeout", type=int, default=60,
                   help="Seconds before unloading idle model (0=never)")
    p.add_argument("--target-latency-ms", type=int, default=500,
                   help="Target end-to-end latency in milliseconds")
    
    # Monitoring
    p.add_argument("--metrics", action="store_true",
                   help="Enable detailed performance metrics")
    p.add_argument("--metrics-log", type=str, default=None,
                   help="File to log metrics (JSON lines)")
    p.add_argument("--metrics-summary-every", type=int, default=10,
                   help="Print summary every N transcriptions")
    
    # Utility
    p.add_argument("--print-devices", action="store_true",
                   help="List available audio input devices")
    
    return p

def main():
    """Main entry point for optimized CLI"""
    args = build_argparser().parse_args()
    
    # Handle device listing
    if args.print_devices:
        devices = list_input_devices()
        if not devices:
            print("No input devices found.", file=sys.stderr)
            sys.exit(1)
        for d in devices:
            print(f"[{d['index']:02d}] {d['name']} (sr={d['default_samplerate']})")
        return
    
    # Load configuration
    config = load_config()
    
    # Parse hotkey chord
    chord = parse_chord(args.chord)
    if not chord:
        print(f"Invalid chord: {args.chord}", file=sys.stderr)
        sys.exit(2)
    
    # Initialize performance tracker
    tracker = None
    if args.metrics:
        tracker = PerformanceTracker(
            target_latency_ms=args.target_latency_ms,
            log_file=args.metrics_log,
            print_summary_every=args.metrics_summary_every
        )
    
    # Initialize audio recorder with optimizations
    print("🎤 Initializing optimized audio recorder...", file=sys.stderr)
    recorder = OptimizedVoiceRecorder(
        device_name=args.device,
        max_sec=args.max_sec,
        buffer_ms=args.buffer_ms,
        use_vad=args.use_vad,
        vad_config={
            'aggressiveness': args.vad_aggressiveness,
            'frame_ms': args.vad_frame_ms,
            'hangover_ms': args.vad_hangover_ms,
            'min_utterance_ms': args.vad_min_utterance_ms,
            'max_utterance_ms': args.vad_max_utterance_ms
        } if args.use_vad else None
    )
    
    # Initialize transcriber with optimizations
    print(f"⚡ Loading optimized Whisper model: {args.model}", file=sys.stderr)
    
    # Load custom words if provided
    custom_words = {}
    if args.custom_words:
        try:
            with open(args.custom_words, 'r') as f:
                custom_words = json.load(f)
            print(f"📖 Loaded {len(custom_words)} custom word replacements", file=sys.stderr)
        except Exception as e:
            print(f"⚠️  Failed to load custom words: {e}", file=sys.stderr)
    
    transcriber = OptimizedTranscriber(
        model_name=args.model,
        lang='en' if args.model.endswith('.en') else args.lang,
        idle_timeout_seconds=args.idle_timeout,
        custom_words=custom_words,
        warmup=args.warmup
    )
    
    # Initialize text inserter
    inserter = TextInserter(
        use_ax_api=args.use_ax_api,
        paste_delay_ms=args.paste_delay_ms
    )
    
    # Initialize assistant if enabled
    assistant = None
    if args.assistant_mode:
        print(f"🤖 Initializing Assistant Mode...", file=sys.stderr)
        assistant = Assistant(model_name=args.assistant_model)
        assistant.enable()
        if assistant.enabled:
            print(f"✅ Assistant Mode enabled", file=sys.stderr)
        else:
            print(f"⚠️  Assistant Mode failed to initialize", file=sys.stderr)
            assistant = None
    
    # Print configuration summary
    print("\n" + "="*60, file=sys.stderr)
    print("⚡ OPTIMIZED LOCAL DICTATION", file=sys.stderr)
    print("="*60, file=sys.stderr)
    print(f"Model: {args.model}", file=sys.stderr)
    print(f"Hotkey: {args.chord}", file=sys.stderr)
    print(f"  • Single press-hold: Push-to-talk", file=sys.stderr)
    print(f"  • Double-tap: Toggle hands-free mode", file=sys.stderr)
    print(f"Debounce: {args.debounce_ms}ms", file=sys.stderr)
    
    if args.use_vad:
        print(f"VAD: Enabled (aggr={args.vad_aggressiveness}, "
              f"hangover={args.vad_hangover_ms}ms)", file=sys.stderr)
    
    if args.use_ax_api:
        print(f"Text insertion: Accessibility API (fastest)", file=sys.stderr)
    else:
        print(f"Text insertion: Clipboard (compatible)", file=sys.stderr)
    
    if args.metrics:
        print(f"Metrics: Enabled (target={args.target_latency_ms}ms)", file=sys.stderr)
    
    print("="*60, file=sys.stderr)
    print("Ready! Press hotkey to start.", file=sys.stderr)
    print("="*60 + "\n", file=sys.stderr)
    
    # State tracking
    current_mode = RecordingMode.IDLE
    recording_start_time = None
    
    def on_mode_change(mode: RecordingMode):
        """Handle recording mode changes (hands-free)"""
        nonlocal current_mode, recording_start_time
        current_mode = mode
        
        if mode == RecordingMode.ARMED:
            # Start listening for voice
            recorder.start(hands_free=True)
            
            # Set up VAD callbacks
            def on_voice_start():
                nonlocal recording_start_time
                recording_start_time = time.perf_counter()
                recorder.start_recording()
            
            def on_voice_end():
                # Voice ended, process audio
                process_recording(is_hands_free=True)
            
            recorder.on_voice_start = on_voice_start
            recorder.on_voice_end = on_voice_end
            
        elif mode == RecordingMode.IDLE:
            # Stop hands-free mode
            if recorder.is_active():
                recorder.stop()
    
    def on_push_to_talk(active: bool):
        """Handle push-to-talk activation"""
        nonlocal recording_start_time
        
        if active:
            # Start recording
            recording_start_time = time.perf_counter()
            recorder.start(hands_free=False)
        else:
            # Stop and process
            process_recording(is_hands_free=False)
    
    def process_recording(is_hands_free: bool):
        """Process recorded audio and insert text"""
        nonlocal recording_start_time
        
        try:
            # Timing
            recording_end = time.perf_counter()
            recording_duration = (recording_end - recording_start_time) * 1000 if recording_start_time else 0
            
            # Stop recording and get audio
            if tracker:
                tracker.start_timer('audio_processing')
            audio = recorder.stop()
            audio_processing_time = tracker.end_timer('audio_processing') if tracker else 0
            
            if audio is None or audio.size == 0:
                print("(no audio captured)", file=sys.stderr)
                return
            
            # VAD processing time (if used)
            vad_processing_time = 0  # Already included in audio processing for now
            
            # Transcribe
            if tracker:
                tracker.start_timer('transcription')
            text = transcriber.transcribe(
                audio,
                output=args.output,
                measure_time=args.metrics
            )
            transcription_time = tracker.end_timer('transcription') if tracker else 0
            
            if not text:
                print("(no speech detected)", file=sys.stderr)
                return
            
            # Process with assistant if enabled
            if assistant and assistant.enabled:
                if assistant.process_transcription(text):
                    # Command was processed
                    if tracker:
                        tracker.add_transcription(
                            recording_duration_ms=recording_duration,
                            vad_processing_ms=vad_processing_time,
                            audio_processing_ms=audio_processing_time,
                            transcription_ms=transcription_time,
                            text_insertion_ms=0,  # No insertion for commands
                            audio_samples=audio.size,
                            text=text,
                            mode='hands_free' if is_hands_free else 'push_to_talk',
                            model=args.model,
                            success=True
                        )
                    return
                
                # Format for app context
                text = assistant.format_for_app_context(text)
            
            # Insert text
            if tracker:
                tracker.start_timer('text_insertion')
            success = inserter.insert_text(text, measure_time=args.metrics)
            text_insertion_time = tracker.end_timer('text_insertion') if tracker else 0
            
            # Record metrics
            if tracker:
                tracker.add_transcription(
                    recording_duration_ms=recording_duration,
                    vad_processing_ms=vad_processing_time,
                    audio_processing_ms=audio_processing_time,
                    transcription_ms=transcription_time,
                    text_insertion_ms=text_insertion_time,
                    audio_samples=audio.size,
                    text=text,
                    mode='hands_free' if is_hands_free else 'push_to_talk',
                    model=args.model,
                    success=success
                )
            
            # Re-arm hands-free mode if active
            if is_hands_free and current_mode == RecordingMode.ARMED:
                recorder.start(hands_free=True)
                
        except Exception as e:
            print(f"Error processing recording: {e}", file=sys.stderr)
    
    # Initialize hotkey listener
    listener = EnhancedHotkeyListener(
        chord=chord,
        debounce_ms=args.debounce_ms,
        double_tap_ms=args.double_tap_ms,
        on_mode_change=on_mode_change,
        on_push_to_talk=on_push_to_talk
    )
    
    try:
        listener.start()
        listener.join()
    except KeyboardInterrupt:
        listener.stop()
        print("\n\nStopping...", file=sys.stderr)
        
        # Print final metrics
        if tracker:
            tracker.print_summary()
            tracker.print_optimization_tips()
            
            # Export metrics if log file specified
            if args.metrics_log:
                export_path = args.metrics_log.replace('.log', '_summary.json')
                tracker.export_metrics(export_path)
        
        # Print component metrics
        print("\n📊 Component Metrics:", file=sys.stderr)
        print(f"  Recorder: {recorder.get_metrics()}", file=sys.stderr)
        print(f"  Transcriber: {transcriber.get_metrics()}", file=sys.stderr)
        print(f"  Inserter: {inserter.get_metrics()}", file=sys.stderr)
        
        print("\n✅ Session ended.", file=sys.stderr)

if __name__ == "__main__":
    main()