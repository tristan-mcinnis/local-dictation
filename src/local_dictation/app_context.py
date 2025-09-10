"""
App context detection for intelligent formatting.
Detects the active application and applies context-specific formatting.
"""

import logging
import subprocess
from typing import Optional
from .config import get_email_sign_off, is_email_formatting_enabled

logger = logging.getLogger(__name__)

def get_active_app() -> Optional[str]:
    """
    Get the name of the currently active application on macOS.
    Returns None if detection fails.
    """
    try:
        # Use AppleScript to get the frontmost application
        script = 'tell application "System Events" to get name of first application process whose frontmost is true'
        result = subprocess.run(
            ['osascript', '-e', script],
            capture_output=True,
            text=True,
            timeout=1
        )
        
        if result.returncode == 0:
            app_name = result.stdout.strip()
            logger.debug(f"Active app detected: {app_name}")
            return app_name
        else:
            logger.warning("Failed to detect active app")
            return None
            
    except Exception as e:
        logger.error(f"Error detecting active app: {e}")
        return None

def is_outlook_active() -> bool:
    """Check if Microsoft Outlook is the active application."""
    app_name = get_active_app()
    if app_name:
        # Check for various Outlook app names
        outlook_names = ['Microsoft Outlook', 'Outlook', 'Mail']
        return any(outlook in app_name for outlook in outlook_names)
    return False

def format_as_email(text: str, sign_off: Optional[str] = None) -> str:
    """
    Format transcribed text as a professional email.
    
    Args:
        text: The raw transcribed text
        sign_off: The email signature to append (loads from config if not provided)
    
    Returns:
        Formatted email text with proper structure and sign-off
    """
    if sign_off is None:
        sign_off = get_email_sign_off()
    # The prompt for the LLM to format as email
    prompt = f"""You are an intelligent text formatter specializing in cleaning up transcribed speech. Your task is to transform raw transcribed text into well-formatted email content while maintaining the speaker's original intent and voice.

CORE PRINCIPLES:
• Authenticity - Keep the original wording and phrasing as much as possible
• Intelligent corrections - Make corrections only where needed for comprehension
• Readability - Apply proper formatting, punctuation, and structure

EMAIL FORMATTING GUIDELINES:

Punctuation & Grammar:
- Add appropriate punctuation (periods, commas, question marks)
- Correct obvious transcription errors while preserving speaking style
- Fix run-on sentences by adding natural breaks
- Maintain conversational tone and personal speaking patterns

Structure & Organization:
- Create paragraph breaks at natural topic transitions
- Use bullet points or numbered lists when the speaker is listing items
- Keep paragraphs concise (2-4 sentences typically)
- Add appropriate line breaks between paragraphs

Preserve Original Intent:
- Keep colloquialisms and regional expressions
- Maintain the speaker's vocabulary level
- Don't "upgrade" simple language to sound more sophisticated
- Preserve humor, sarcasm, and emotional tone

OUTPUT FORMAT:
Return ONLY the formatted email body text with:
- Clear paragraph breaks (double newlines between paragraphs)
- Proper punctuation and capitalization
- Any structural elements (lists) that improve clarity
- Original meaning and voice intact
- NO greeting line (no "Hi" or "Dear")
- NO sign-off (no "Best regards" etc)

Remember: You're formatting for email while keeping the original voice. Make it professional yet authentic.

Here is the text to format:
<text>
{text}
</text>

Return only the formatted email body:"""
    
    return prompt

def get_formatting_prompt(text: str, app_name: Optional[str] = None, sign_off: Optional[str] = None) -> str:
    """
    Get the appropriate formatting prompt based on the active application.
    
    Args:
        text: The transcribed text to format
        app_name: The name of the active app (if None, will detect)
        sign_off: Optional email sign-off (uses config if not provided)
    
    Returns:
        The formatting prompt or None if no special formatting needed
    """
    # Check if email formatting is enabled
    if not is_email_formatting_enabled():
        return None
    
    if app_name is None:
        app_name = get_active_app()
    
    if app_name and any(outlook in app_name for outlook in ['Microsoft Outlook', 'Outlook', 'Mail']):
        logger.info("Email app detected - will format as email")
        return format_as_email(text, sign_off)
    
    # For all other apps, return None (no special formatting)
    return None