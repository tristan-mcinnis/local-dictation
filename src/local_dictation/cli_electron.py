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
from .config import load_config
from .app_context import get_formatting_prompt

def env_or(name: str, default: str | None):
    return os.getenv(name, default)


def env_flag(name: str) -> bool | None:
    value = os.getenv(name)
    if value is None:
        return None
    return value.lower() in {"1", "true", "yes", "on"}


def env_float(name: str) -> float | None:
    value = os.getenv(name)
    if value is None:
        return None
    try:
        return float(value)
    except ValueError:
        return None


def env_int(name: str) -> int | None:
    value = os.getenv(name)
    if value is None:
        return None
    try:
        return int(value)
    except ValueError:
        return None

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
    p.add_argument("--assistant-provider", choices=["mlx", "openai"],
                   default=env_or("ASSISTANT_PROVIDER", None),
                   help="Assistant backend to use: local MLX or OpenAI GPT-5")
    p.add_argument("--assistant-model", default=env_or("ASSISTANT_MODEL", None),
                   help="MLX model for assistant mode (provider=mlx)")
    p.add_argument("--assistant-openai-model", default=env_or("ASSISTANT_OPENAI_MODEL", None),
                   help="OpenAI model when provider=openai (gpt-5, gpt-5-mini, gpt-5-nano)")
    p.add_argument("--assistant-openai-key", default=env_or("ASSISTANT_OPENAI_KEY", None),
                   help="OpenAI API key (optional if set in environment)")
    p.add_argument("--assistant-openai-key-env", default=env_or("ASSISTANT_OPENAI_KEY_ENV", None),
                   help="Environment variable name for the OpenAI API key")
    p.add_argument("--assistant-openai-organization", default=env_or("ASSISTANT_OPENAI_ORG", None),
                   help="OpenAI organization ID (optional)")
    p.add_argument("--assistant-openai-base-url", default=env_or("ASSISTANT_OPENAI_BASE_URL", None),
                   help="Custom OpenAI-compatible base URL")
    p.add_argument("--assistant-result-action",
                   choices=["auto", "replace_selection", "copy_to_clipboard", "show_textedit"],
                   default=env_or("ASSISTANT_RESULT_ACTION", None),
                   help="How to deliver assistant results (default: auto)")
    p.add_argument("--assistant-temperature", type=float, default=None,
                   help="Sampling temperature for assistant responses")
    p.add_argument("--assistant-max-output-tokens", type=int, default=None,
                   help="Maximum token count for assistant responses")
    p.add_argument("--assistant-copy-result", dest="assistant_copy_result", action="store_true",
                   help="Always copy assistant output to the clipboard")
    p.add_argument("--assistant-no-copy-result", dest="assistant_copy_result", action="store_false",
                   help="Do not leave assistant output on the clipboard after replacing text")
    p.set_defaults(assistant_copy_result=None)
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

    config = load_config()

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
    
    assistant_config = config.get('assistant', {})

    # Initialize assistant if enabled
    assistant = None
    assistant_provider_ready = None
    assistant_model_requested = None
    email_formatting = os.getenv('EMAIL_FORMATTING', 'true').lower() == 'true'
    email_sign_off = os.getenv('EMAIL_SIGN_OFF', 'Best regards,\n[Your Name]')

    if args.assistant_mode:
        assistant_provider = (args.assistant_provider or assistant_config.get('provider') or 'mlx').lower()
        openai_model = (
            args.assistant_openai_model
            or assistant_config.get('openai_model')
            or ('gpt-5-mini' if assistant_provider == 'openai' else None)
        )

        if assistant_provider == 'openai':
            model_name = openai_model or 'gpt-5-mini'
        else:
            model_name = (
                args.assistant_model
                or assistant_config.get('model')
                or 'mlx-community/Llama-3.2-3B-Instruct-4bit'
            )

        result_action = args.assistant_result_action or assistant_config.get('result_action', 'auto')

        copy_result = assistant_config.get('copy_result_to_clipboard', True)
        copy_env = env_flag('ASSISTANT_COPY_RESULT')
        if copy_env is not None:
            copy_result = copy_env
        if args.assistant_copy_result is not None:
            copy_result = args.assistant_copy_result

        temperature = assistant_config.get('temperature', 0.2)
        temp_env = env_float('ASSISTANT_TEMPERATURE')
        if temp_env is not None:
            temperature = temp_env
        if args.assistant_temperature is not None:
            temperature = args.assistant_temperature

        max_tokens = assistant_config.get('max_output_tokens', 900)
        max_env = env_int('ASSISTANT_MAX_OUTPUT_TOKENS')
        if max_env is not None:
            max_tokens = max_env
        if args.assistant_max_output_tokens is not None:
            max_tokens = args.assistant_max_output_tokens

        openai_api_key = args.assistant_openai_key or assistant_config.get('openai_api_key')
        openai_key_env = args.assistant_openai_key_env or assistant_config.get('openai_api_key_env', 'OPENAI_API_KEY')
        openai_org = args.assistant_openai_organization or assistant_config.get('openai_organization')
        openai_base_url = args.assistant_openai_base_url or assistant_config.get('openai_base_url')

        assistant_kwargs = {
            'model_name': model_name,
            'provider': assistant_provider,
            'openai_model': openai_model,
            'openai_api_key': openai_api_key,
            'openai_api_key_env': openai_key_env,
            'openai_organization': openai_org,
            'openai_base_url': openai_base_url,
            'result_action': result_action,
            'copy_result_to_clipboard': copy_result,
            'temperature': temperature,
            'max_output_tokens': max_tokens,
            'system_prompt': assistant_config.get('system_prompt'),
            'more_info_prompt': assistant_config.get('more_info_prompt'),
        }

        assistant_provider_ready = assistant_provider
        assistant_model_requested = model_name

        assistant = Assistant(**assistant_kwargs)
        assistant.enable()

        # Report assistant status
        if assistant.enabled:
            send_message("ASSISTANT_MODE", "ready")
            print(f"✅ Assistant model loaded: {assistant.model_name}", file=sys.stderr)
            if assistant.provider == "openai":
                print("   Using OpenAI Responses API (gpt-5 / gpt-5-mini / gpt-5-nano)", file=sys.stderr)
        else:
            send_message("ASSISTANT_MODE", "failed")
            print(f"❌ Assistant model failed to load: {model_name}", file=sys.stderr)
            if assistant_provider == 'openai':
                print(f"   Verify that the OpenAI API key is available via {openai_key_env}.", file=sys.stderr)
            print("   Commands will fall back to regular dictation", file=sys.stderr)

    # Send ready signal with actual VAD status
    assistant_ready = assistant.enabled if assistant else False
    assistant_model_info = None
    assistant_provider_info = None
    if args.assistant_mode:
        assistant_provider_info = assistant_provider_ready
        assistant_model_info = assistant.model_name if assistant_ready else assistant_model_requested

    send_message("READY", json.dumps({
        "model": args.model,
        "chord": args.chord,
        "device_rate": rec.samplerate,
        "needs_resample": rec.needs_resample,
        "assistant_mode": args.assistant_mode,
        "assistant_provider": assistant_provider_info,
        "assistant_model": assistant_model_info,
        "assistant_ready": assistant_ready,
        "vad_enabled": vad_actually_enabled,
        "idle_timeout": args.idle_timeout,
        "custom_words_loaded": len(custom_words) if custom_words else 0
    }))

    # Create a keyboard controller for typing
    kbd = keyboard.Controller()

    # Track processing state to prevent duplicates
    processing_state = {"active": False, "last_text": "", "last_time": 0}

    def on_chord(active: bool):
        try:
            if active:
                send_message("RECORDING_START")
                rec.start()
            else:
                send_message("RECORDING_STOP")

                audio = rec.stop()

                if audio is not None and audio.size > 0:
                    try:
                        text = tx.transcribe(audio, output="text")
                    except Exception as e:
                        # Handle GPU/Metal resource conflicts gracefully
                        error_msg = str(e)
                        if "metal" in error_msg.lower() or "gpu" in error_msg.lower() or "ggml" in error_msg.lower():
                            send_message("ERROR", "GPU resources busy (Ollama running?). Please restart app.")
                            print(f"GPU resource conflict detected: {error_msg}", file=sys.stderr)
                        else:
                            send_message("ERROR", f"Transcription failed: {error_msg}")
                            print(f"Transcription error: {error_msg}", file=sys.stderr)
                        return

                    if text:
                        # Prevent duplicate processing of the same text within a short time window
                        current_time = time.time()
                        if (processing_state["last_text"] == text and
                            current_time - processing_state["last_time"] < 2.0):
                            print(f"Skipping duplicate text: {text[:50]}...", file=sys.stderr)
                            return

                        processing_state["last_text"] = text
                        processing_state["last_time"] = current_time

                        # Send transcript to Electron for saving
                        send_message("TRANSCRIPT", text)

                        # In assistant mode, try to process as command first
                        if assistant and assistant.process_transcription(text):
                            send_message("COMMAND_PROCESSED", text)
                        else:
                            # Check if it looked like a command but failed
                            if assistant and assistant.enabled:
                                command_type, _ = assistant.parse_command(text)
                                if command_type:
                                    send_message("COMMAND_FAILED", f"Command detected but failed: {command_type}")
                                    print(f"⚠️ Command '{command_type}' detected but failed to execute", file=sys.stderr)
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
