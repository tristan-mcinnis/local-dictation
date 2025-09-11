#!/usr/bin/env python3
"""
Enhanced Hotkey Listener with Hands-Free Mode
- Double-tap detection for hands-free mode
- Optimized for low latency
- State machine for mode transitions
"""
from __future__ import annotations
from pynput import keyboard
import threading
import time
from enum import Enum
from typing import Callable, Optional, Set

class RecordingMode(Enum):
    IDLE = "idle"
    ARMED = "armed"  # Ready to detect voice
    RECORDING = "recording"
    ENDPOINT = "endpoint"
    PROCESSING = "processing"

KEY_MAP = {
    "CMD": keyboard.Key.cmd,
    "ALT": keyboard.Key.alt,
    "OPT": keyboard.Key.alt,
    "CTRL": keyboard.Key.ctrl,
    "SHIFT": keyboard.Key.shift,
    "LALT": keyboard.Key.alt,
    "RALT": keyboard.Key.alt,
    "LCMD": keyboard.Key.cmd,
    "RCMD": keyboard.Key.cmd,
    "LCTRL": keyboard.Key.ctrl,
    "RCTRL": keyboard.Key.ctrl,
    "LSHIFT": keyboard.Key.shift,
    "RSHIFT": keyboard.Key.shift,
}

def parse_chord(chord_str: str | None) -> set:
    if not chord_str:
        return {keyboard.Key.cmd, keyboard.Key.alt}
    parts = [p.strip().upper() for p in chord_str.replace(",", " ").split() if p.strip()]
    chord = set()
    for p in parts:
        if len(p) == 1:
            chord.add(keyboard.KeyCode.from_char(p.lower()))
        elif p.startswith("F") and p[1:].isdigit():
            chord.add(getattr(keyboard.Key, p.lower()))
        else:
            chord.add(KEY_MAP.get(p, None) or getattr(keyboard.Key, p.lower(), None))
    return {k for k in chord if k is not None}

class EnhancedHotkeyListener:
    """
    Enhanced hotkey listener with hands-free mode support
    - Double-tap toggles hands-free mode
    - Single press-hold for push-to-talk
    - Optimized for minimal latency
    """
    
    def __init__(self, 
                 chord: Set,
                 debounce_ms: int = 30,
                 double_tap_ms: int = 500,
                 on_mode_change: Optional[Callable[[RecordingMode], None]] = None,
                 on_push_to_talk: Optional[Callable[[bool], None]] = None):
        """
        Initialize enhanced hotkey listener
        
        Args:
            chord: Set of keys for activation
            debounce_ms: Debounce time for key release (default 30ms)
            double_tap_ms: Maximum time between taps for double-tap detection
            on_mode_change: Callback for hands-free mode changes
            on_push_to_talk: Callback for push-to-talk activation
        """
        self.chord = chord
        self.debounce_ms = debounce_ms
        self.double_tap_ms = double_tap_ms
        self.on_mode_change = on_mode_change or (lambda mode: None)
        self.on_push_to_talk = on_push_to_talk or (lambda active: None)
        
        # State tracking
        self._pressed = set()
        self._push_to_talk_active = False
        self._hands_free_mode = RecordingMode.IDLE
        self._lock = threading.Lock()
        
        # Timing for double-tap detection
        self._last_tap_time = 0
        self._tap_count = 0
        self._release_deadline = None
        
        # Timing for push-to-talk
        self._press_start_time = None
        self._is_long_press = False
        
        # Keyboard listener
        self._listener = keyboard.Listener(
            on_press=self._on_press,
            on_release=self._on_release
        )
        
        # Fast ticker thread for timing
        self._ticker_stop = threading.Event()
        self._ticker = threading.Thread(target=self._tick, daemon=True)
    
    def start(self):
        """Start the listener"""
        self._listener.start()
        self._ticker.start()
    
    def join(self):
        """Wait for listener to stop"""
        self._listener.join()
        self._ticker_stop.set()
        self._ticker.join()
    
    def stop(self):
        """Stop the listener"""
        self._listener.stop()
        self._ticker_stop.set()
    
    def get_mode(self) -> RecordingMode:
        """Get current recording mode"""
        with self._lock:
            return self._hands_free_mode
    
    def set_mode(self, mode: RecordingMode):
        """Set recording mode (for external control)"""
        with self._lock:
            if self._hands_free_mode != mode:
                self._hands_free_mode = mode
                self.on_mode_change(mode)
    
    def _now_ms(self) -> int:
        """Get current time in milliseconds"""
        return int(time.perf_counter() * 1000)
    
    def _on_press(self, key):
        """Handle key press events"""
        with self._lock:
            self._pressed.add(key)
            
            # Check if chord is pressed
            if self.chord.issubset(self._pressed):
                if not self._push_to_talk_active:
                    # New press
                    self._press_start_time = self._now_ms()
                    self._is_long_press = False
                    self._release_deadline = None
                    
                    # Start push-to-talk immediately
                    self._push_to_talk_active = True
                    if self._hands_free_mode == RecordingMode.IDLE:
                        self.on_push_to_talk(True)
    
    def _on_release(self, key):
        """Handle key release events"""
        with self._lock:
            self._pressed.discard(key)
            
            # Check if chord was released
            if self._push_to_talk_active and not self.chord.issubset(self._pressed):
                # Schedule delayed release (debounce)
                if self._release_deadline is None:
                    self._release_deadline = self._now_ms() + self.debounce_ms
    
    def _handle_release(self):
        """Process key release after debounce"""
        now = self._now_ms()
        press_duration = now - self._press_start_time if self._press_start_time else 0
        
        # Check if this was a long press (push-to-talk)
        if press_duration > 150:  # 150ms threshold for long press
            self._is_long_press = True
            if self._hands_free_mode == RecordingMode.IDLE:
                self.on_push_to_talk(False)
        else:
            # Short press - check for double-tap
            time_since_last_tap = now - self._last_tap_time
            
            if time_since_last_tap < self.double_tap_ms:
                # Double-tap detected!
                self._tap_count = 0
                self._toggle_hands_free_mode()
            else:
                # Single tap - reset counter
                self._tap_count = 1
                self._last_tap_time = now
        
        self._push_to_talk_active = False
        self._press_start_time = None
    
    def _toggle_hands_free_mode(self):
        """Toggle hands-free mode on/off"""
        if self._hands_free_mode == RecordingMode.IDLE:
            # Enter hands-free mode
            self._hands_free_mode = RecordingMode.ARMED
            print("🎤 Hands-Free Mode: ACTIVATED (double-tap again to stop)", file=sys.stderr)
        else:
            # Exit hands-free mode
            self._hands_free_mode = RecordingMode.IDLE
            print("🔇 Hands-Free Mode: DEACTIVATED", file=sys.stderr)
        
        self.on_mode_change(self._hands_free_mode)
    
    def _tick(self):
        """Fast ticker for timing and state management"""
        while not self._ticker_stop.is_set():
            with self._lock:
                now = self._now_ms()
                
                # Handle debounced key release
                if self._push_to_talk_active and self._release_deadline:
                    if now >= self._release_deadline:
                        self._release_deadline = None
                        self._handle_release()
                
                # Clear old tap counter
                if self._tap_count > 0 and (now - self._last_tap_time) > self.double_tap_ms:
                    self._tap_count = 0
            
            time.sleep(0.003)  # 3ms for ultra-low latency