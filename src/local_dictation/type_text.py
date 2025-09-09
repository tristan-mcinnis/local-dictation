#!/usr/bin/env python3
"""
Text typing utilities for macOS
"""
import subprocess
import sys
import time

def type_with_applescript(text):
    """Type text using AppleScript (macOS only)"""
    # Escape special characters for AppleScript
    text = text.replace('\\', '\\\\')
    text = text.replace('"', '\\"')
    
    script = f'''
    tell application "System Events"
        keystroke "{text}"
    end tell
    '''
    
    try:
        subprocess.run(['osascript', '-e', script], check=True, capture_output=True)
        return True
    except subprocess.CalledProcessError as e:
        print(f"AppleScript error: {e}", file=sys.stderr)
        return False

def type_with_pynput(text, kbd):
    """Type text using pynput keyboard controller"""
    try:
        kbd.type(text)
        return True
    except Exception as e:
        print(f"Pynput error: {e}", file=sys.stderr)
        return False

def type_text(text, kbd=None):
    """Try multiple methods to type text"""
    # Method 1: Try pynput first if keyboard controller provided
    if kbd:
        if type_with_pynput(text, kbd):
            return True
    
    # Method 2: Fall back to AppleScript on macOS
    if sys.platform == 'darwin':
        return type_with_applescript(text)
    
    return False