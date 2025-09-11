#!/usr/bin/env python3
"""
Direct text insertion using macOS Accessibility API
- Faster than clipboard paste
- Direct insertion into text fields
- Fallback to clipboard for compatibility
"""
from __future__ import annotations
import subprocess
import time
import pyperclip
from typing import Optional
import sys

class TextInserter:
    """
    Fast text insertion using multiple methods
    - Primary: Direct Accessibility API insertion
    - Fallback: Optimized clipboard paste
    """
    
    def __init__(self, use_ax_api: bool = True, paste_delay_ms: int = 10):
        """
        Initialize text inserter
        
        Args:
            use_ax_api: Try to use Accessibility API first
            paste_delay_ms: Delay after clipboard write before paste
        """
        self.use_ax_api = use_ax_api
        self.paste_delay_ms = paste_delay_ms
        
        # Performance tracking
        self.total_insertions = 0
        self.total_time = 0.0
        self.ax_success_count = 0
        self.clipboard_count = 0
    
    def insert_text(self, text: str, measure_time: bool = False) -> bool:
        """
        Insert text at cursor position using fastest available method
        
        Args:
            text: Text to insert
            measure_time: Whether to measure insertion time
            
        Returns:
            True if successful, False otherwise
        """
        if not text:
            return True
        
        start_time = time.perf_counter() if measure_time else 0
        success = False
        
        # Try Accessibility API first
        if self.use_ax_api:
            success = self._insert_via_ax_api(text)
            if success:
                self.ax_success_count += 1
        
        # Fallback to clipboard
        if not success:
            success = self._insert_via_clipboard(text)
            if success:
                self.clipboard_count += 1
        
        # Track performance
        if measure_time and success:
            elapsed = time.perf_counter() - start_time
            self.total_insertions += 1
            self.total_time += elapsed
            method = "AX API" if self.ax_success_count == self.total_insertions else "Clipboard"
            print(f"⚡ Text insertion ({method}): {elapsed*1000:.0f}ms", file=sys.stderr)
        
        return success
    
    def _insert_via_ax_api(self, text: str) -> bool:
        """
        Insert text directly using Accessibility API
        
        This uses AppleScript to directly set the value of the focused text field,
        which is much faster than simulating typing or clipboard paste.
        """
        try:
            # Escape text for AppleScript
            escaped_text = text.replace('\\', '\\\\').replace('"', '\\"')
            
            # AppleScript to insert text directly
            applescript = f'''
            tell application "System Events"
                set frontApp to name of first application process whose frontmost is true
                tell process frontApp
                    try
                        -- Try to get the focused UI element
                        set focusedElement to value of attribute "AXFocusedUIElement" of it
                        
                        -- Check if it's a text field
                        if role of focusedElement is in {{"text field", "text area", "combo box", "search field"}} then
                            -- Get current value and cursor position
                            set currentValue to value of focusedElement
                            
                            -- Try to get selection range
                            try
                                set selRange to value of attribute "AXSelectedTextRange" of focusedElement
                                set selStart to item 1 of selRange
                                set selLength to item 2 of selRange
                                
                                -- Build new text with insertion
                                if currentValue is missing value then
                                    set currentValue to ""
                                end if
                                
                                if selLength > 0 then
                                    -- Replace selection
                                    if selStart > 0 then
                                        set beforeText to text 1 thru selStart of currentValue
                                    else
                                        set beforeText to ""
                                    end if
                                    
                                    if (selStart + selLength) < length of currentValue then
                                        set afterText to text (selStart + selLength + 1) thru -1 of currentValue
                                    else
                                        set afterText to ""
                                    end if
                                    
                                    set value of focusedElement to beforeText & "{escaped_text}" & afterText
                                else
                                    -- Insert at cursor
                                    if selStart > 0 then
                                        set beforeText to text 1 thru selStart of currentValue
                                    else
                                        set beforeText to ""
                                    end if
                                    
                                    if selStart < length of currentValue then
                                        set afterText to text (selStart + 1) thru -1 of currentValue
                                    else
                                        set afterText to ""
                                    end if
                                    
                                    set value of focusedElement to beforeText & "{escaped_text}" & afterText
                                end if
                                
                                -- Update cursor position
                                set newPosition to selStart + (length of "{escaped_text}")
                                set value of attribute "AXSelectedTextRange" of focusedElement to {{newPosition, 0}}
                                
                                return true
                            on error
                                -- Fallback: just append text
                                set value of focusedElement to currentValue & "{escaped_text}"
                                return true
                            end try
                        else
                            return false
                        end if
                    on error
                        return false
                    end try
                end tell
            end tell
            '''
            
            # Execute AppleScript
            result = subprocess.run(
                ['osascript', '-e', applescript],
                capture_output=True,
                text=True,
                timeout=0.5  # 500ms timeout
            )
            
            return result.returncode == 0 and result.stdout.strip() == 'true'
            
        except (subprocess.TimeoutExpired, Exception):
            return False
    
    def _insert_via_clipboard(self, text: str) -> bool:
        """
        Insert text via clipboard (fallback method)
        
        This is more compatible but slower than direct insertion.
        """
        try:
            # Save current clipboard
            old_clipboard = pyperclip.paste()
            
            # Set new text
            pyperclip.copy(text)
            
            # Small delay for clipboard to update
            if self.paste_delay_ms > 0:
                time.sleep(self.paste_delay_ms / 1000.0)
            
            # Paste using Cmd+V
            applescript = '''
            tell application "System Events"
                keystroke "v" using command down
            end tell
            '''
            
            subprocess.run(
                ['osascript', '-e', applescript],
                capture_output=True,
                timeout=0.5
            )
            
            # Restore old clipboard after a delay
            # (Do this in background to not add latency)
            import threading
            def restore():
                time.sleep(0.1)
                try:
                    pyperclip.copy(old_clipboard)
                except:
                    pass
            
            threading.Thread(target=restore, daemon=True).start()
            
            return True
            
        except Exception:
            return False
    
    def type_text(self, text: str) -> bool:
        """
        Type text character by character (slowest method, for compatibility)
        
        This should only be used as a last resort as it's much slower.
        """
        try:
            # Use pynput for typing
            from pynput import keyboard
            controller = keyboard.Controller()
            controller.type(text)
            return True
        except Exception:
            return False
    
    def get_metrics(self) -> dict:
        """Get performance metrics"""
        if self.total_insertions == 0:
            return {
                'total_insertions': 0,
                'avg_time_ms': 0,
                'ax_api_success_rate': 0,
                'clipboard_fallback_rate': 0
            }
        
        return {
            'total_insertions': self.total_insertions,
            'avg_time_ms': (self.total_time / self.total_insertions) * 1000,
            'ax_api_success_rate': self.ax_success_count / self.total_insertions,
            'clipboard_fallback_rate': self.clipboard_count / self.total_insertions
        }