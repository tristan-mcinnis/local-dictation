#!/usr/bin/env python3
"""
Electron IPC version of the CLI for integration with the Electron app
"""
from __future__ import annotations
import argparse
import os
import sys
import time
import json
import threading
from pynput import keyboard
from .hotkey import HotkeyListener, parse_chord
from .audio import VoiceRecorder, list_input_devices
from .transcribe_unified import UnifiedTranscriber
from .type_text import type_text
from .assistant import Assistant
from .app_context import get_formatting_prompt

def env_or(name: str, default: str):
    return os.getenv(name, default)

def build_argparser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser("local-dictation-electron", 
                               description="Electron IPC version of local dictation")
    p.add_argument("--model", default=env_or("MODEL", "medium.en"),
                   help="Whisper model")
    p.add_argument("--lang", default=env_or("LANG", "auto"),
                   help="Language code")
    p.add_argument("--chord", default=env_or("CHORD", "CMD,ALT"),
                   help="Chord like 'CMD,ALT' or 'CTRL,SHIFT'")
    p.add_argument("--debounce-ms", type=int, default=int(env_or("DEBOUNCE_MS", "50")))
    p.add_argument("--max-sec", type=float, default=float(env_or("MAX_SEC", "90")))
    p.add_argument("--highpass-hz", type=float, default=float(env_or("HIGHPASS_HZ", "0")))
    p.add_argument("--device", default=env_or("AUDIO_DEVICE", None))
    p.add_argument("--assistant-mode", action="store_true",
                   help="Enable assistant mode")
    p.add_argument("--assistant-model", default=env_or("ASSISTANT_MODEL", "mlx-community/Llama-3.2-3B-Instruct-4bit"),
                   help="MLX model for assistant mode")
    p.add_argument("--use-vad", action="store_true",
                   help="Enable VAD (Voice Activity Detection) to filter silence")
    p.add_argument("--idle-timeout", type=int, default=int(env_or("IDLE_TIMEOUT", "60")),
                   help="Seconds before unloading idle model (0=never)")
    p.add_argument("--custom-words", type=str, default=env_or("CUSTOM_WORDS", None),
                   help="JSON file with custom word replacements")
    p.add_argument("--engine", choices=["whisper", "auto"], default=env_or("ENGINE", "whisper"),
                   help="Transcription engine")
    return p

def send_message(msg_type: str, data: str = ""):
    """Send IPC message to Electron"""
    message = f"{msg_type}:{data}" if data else msg_type
    print(message, file=sys.stdout, flush=True)
    sys.stdout.flush()  # Extra flush to ensure message is sent

def main():
    # Suppress all stderr output from whisper.cpp
    import contextlib
    sys.stderr = open(os.devnull, 'w')
    
    args = build_argparser().parse_args()

    chord = parse_chord(args.chord)
    if not chord:
        send_message("ERROR", f"Invalid chord: {args.chord}")
        sys.exit(2)

    # Try to initialize recorder with VAD if requested
    vad_actually_enabled = False
    try:
        rec = VoiceRecorder(device_name=args.device,
                            max_sec=args.max_sec,
                            highpass_hz=args.highpass_hz,
                            channels=1,
                            use_vad=args.use_vad)
        # Check if VAD actually initialized when requested
        vad_actually_enabled = args.use_vad and rec.vad is not None
        if args.use_vad and not vad_actually_enabled:
            send_message("LOG", "VAD requested but failed to initialize - continuing without VAD")
    except Exception as e:
        send_message("ERROR", f"Failed to initialize audio recorder: {e}")
        sys.exit(1)

    # Load custom words if provided
    custom_words = {}
    if args.custom_words:
        try:
            with open(args.custom_words, 'r') as f:
                custom_words = json.load(f)
        except Exception as e:
            send_message("LOG", f"Failed to load custom words: {e}")

    # Force English language for .en models
    lang = 'en' if args.model.endswith('.en') else args.lang
    tx = UnifiedTranscriber(
        engine=args.engine,
        model_name=args.model, 
        lang=lang,
        idle_timeout_seconds=args.idle_timeout,
        custom_words=custom_words)
    
    # Initialize assistant if enabled
    assistant = None
    email_formatting = os.getenv('EMAIL_FORMATTING', 'true').lower() == 'true'
    email_sign_off = os.getenv('EMAIL_SIGN_OFF', 'Best regards,\n[Your Name]')
    
    if args.assistant_mode:
        assistant = Assistant(model_name=args.assistant_model)
        assistant.enable()

    # Send ready signal with actual VAD status
    send_message("READY", json.dumps({
        "model": args.model,
        "chord": args.chord,
        "device_rate": rec.samplerate,
        "needs_resample": rec.needs_resample,
        "assistant_mode": args.assistant_mode,
        "assistant_model": args.assistant_model if args.assistant_mode else None,
        "vad_enabled": vad_actually_enabled,
        "idle_timeout": args.idle_timeout,
        "custom_words_loaded": len(custom_words) if custom_words else 0
    }))

    # Create a keyboard controller for typing
    kbd = keyboard.Controller()
    
    def on_chord(active: bool):
        try:
            if active:
                send_message("RECORDING_START")
                rec.start()
            else:
                send_message("RECORDING_STOP")
                
                audio = rec.stop()
                
                if audio is not None and audio.size > 0:
                    text = tx.transcribe(audio, output="text")
                    
                    if text:
                        # Send transcript to Electron for saving
                        send_message("TRANSCRIPT", text)
                        
                        # In assistant mode, try to process as command first
                        if assistant and assistant.process_transcription(text):
                            send_message("COMMAND_PROCESSED", text)
                        else:
                            # Apply app-aware formatting if assistant is enabled and email formatting is on
                            if assistant and assistant.enabled and email_formatting:
                                text = assistant.format_for_app_context(text, sign_off=email_sign_off)
                            
                            # Regular dictation - type the text
                            # Minimal delay to ensure window focus is back
                            time.sleep(0.05)
                            
                            # Type the text at cursor position
                            if type_text(text, kbd):
                                send_message("TYPED", "success")
                            else:
                                send_message("TYPE_ERROR", "Failed to type text")
        except Exception as e:
            send_message("ERROR", str(e))

    listener = HotkeyListener(chord=chord, debounce_ms=args.debounce_ms, on_chord_active=on_chord)
    
    # Also listen for stdin commands from Electron
    import select
    
    def stdin_listener():
        while True:
            try:
                if sys.stdin in select.select([sys.stdin], [], [], 0)[0]:
                    line = sys.stdin.readline().strip()
                    if line.startswith("TOGGLE_ASSISTANT:"):
                        # Toggle assistant mode from Electron
                        enabled = line.split(":", 1)[1] == "true"
                        if assistant:
                            if enabled:
                                assistant.enable()
                            else:
                                assistant.disable()
                            send_message("ASSISTANT_MODE", "enabled" if enabled else "disabled")
                    elif line == "START":
                        # Manual start from Electron menu
                        on_chord(True)
                        time.sleep(0.1)  # Brief delay
                        on_chord(False)
                    elif line == "QUIT":
                        listener.stop()
                        sys.exit(0)
            except:
                pass
            time.sleep(0.1)
    
    stdin_thread = threading.Thread(target=stdin_listener, daemon=True)
    stdin_thread.start()
    
    try:
        listener.start()
        listener.join()
    except KeyboardInterrupt:
        listener.stop()
        send_message("STOPPED")

if __name__ == "__main__":
    main()