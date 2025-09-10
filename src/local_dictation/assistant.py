"""
Assistant mode functionality using MLX language models.
Processes spoken commands to perform actions on text.
"""

import re
import time
import logging
from typing import Optional, Tuple
from pynput import keyboard
from pynput.keyboard import Key, Controller
import pyperclip
from .app_context import get_formatting_prompt, get_active_app
from .config import get_email_sign_off

logger = logging.getLogger(__name__)

controller = Controller()

class Assistant:
    """Handles assistant mode operations using MLX models."""
    
    def __init__(self, model_name: str = "mlx-community/Llama-3.2-3B-Instruct-4bit"):
        """Initialize the assistant with specified model."""
        self.model_name = model_name
        self.model = None
        self.tokenizer = None
        self.enabled = False
        
        # Validate model name format
        if not "/" in model_name and not model_name.startswith("mlx-community/"):
            self.model_name = f"mlx-community/{model_name}"
        
    def load_model(self):
        """Lazy load the MLX model when first needed."""
        if self.model is None:
            try:
                from mlx_lm import load
                logger.info(f"Loading MLX model: {self.model_name}")
                logger.info("Note: First-time model download may take a few minutes...")
                self.model, self.tokenizer = load(self.model_name)
                logger.info("Model loaded successfully")
            except ImportError:
                logger.error("mlx-lm not installed. Install with: uv add mlx-lm")
                self.enabled = False
                return False
            except Exception as e:
                logger.error(f"Failed to load model: {e}")
                logger.error("Try running again - the model may still be downloading.")
                logger.error("Or check your internet connection and try a different model.")
                self.enabled = False
                return False
        return True
    
    def enable(self):
        """Enable assistant mode."""
        if self.load_model():
            self.enabled = True
            logger.info("Assistant mode enabled")
        else:
            logger.warning("Assistant mode disabled due to model loading failure")
    
    def disable(self):
        """Disable assistant mode."""
        self.enabled = False
        logger.info("Assistant mode disabled")
    
    def parse_command(self, text: str) -> Tuple[Optional[str], Optional[str]]:
        """
        Parse transcribed text to determine if it's a command or regular dictation.
        Returns (command_type, command_args) or (None, None) for regular dictation.
        """
        if not self.enabled:
            return None, None
        
        text_strip = text.strip()
        text_lower = text_strip.lower()
        
        # Command patterns
        command_patterns = {
            'rewrite': [
                r'^rewrite (?:this |that )?(?:in |as |to )?(.+)$',
                r'^make (?:this |that )?(.+)$',
                r'^change (?:this |that )?to (.+)$'
            ],
            'explain': [
                r'^explain (?:this|that)?(.*)$',
                r'^what (?:is|does) (?:this|that)?(.*)$',
                r'^tell me about (?:this|that)?(.*)$'
            ],
            'summarize': [
                r'^summarize (?:this|that)?(.*)$',
                r'^give me a summary(?: of this)?(.*)$'
            ],
            'translate': [
                r'^translate (?:this |that )?(?:to |into )?(.+)$'
            ],
            'fix': [
                r'^fix (?:this|that)?(.*)$',
                r'^correct (?:this|that)?(.*)$'
            ]
        }
        
        for command_type, patterns in command_patterns.items():
            for pattern in patterns:
                match = re.match(pattern, text_lower)
                if match:
                    # Extract args from original text to preserve case
                    start_pos = match.start(1) if match.lastindex else len(text_strip)
                    args = text_strip[start_pos:] if match.lastindex else ""
                    return command_type, args.strip()
        
        return None, None
    
    def get_selected_text(self) -> Optional[str]:
        """Get currently selected text via clipboard."""
        try:
            # Copy selection to clipboard (just like the reference code)
            with controller.pressed(Key.cmd):
                controller.tap("c")
            
            # Get the clipboard string
            time.sleep(0.1)
            text = pyperclip.paste()
            
            if text:
                logger.debug(f"Text copied from selection: {text[:50]}...")
                return text
            else:
                logger.warning("No text in clipboard after copy")
                return None
        except Exception as e:
            logger.error(f"Failed to get selected text: {e}")
            return None
    
    def replace_selected_text(self, new_text: str):
        """Replace selected text with new text."""
        try:
            # Paste the fixed string to clipboard (just like reference code)
            pyperclip.copy(new_text)
            time.sleep(0.1)
            
            # Paste the clipboard and replace the selected text
            with controller.pressed(Key.cmd):
                controller.tap("v")
            
            logger.debug("Replaced text with corrected version")
        except Exception as e:
            logger.error(f"Failed to replace text: {e}")
    
    def generate_response(self, prompt: str) -> str:
        """Generate response using the MLX model."""
        if not self.model:
            if not self.load_model():
                return ""
        
        try:
            from mlx_lm import generate
            
            # Apply chat template if available
            if self.tokenizer.chat_template is not None:
                messages = [{"role": "user", "content": prompt}]
                prompt = self.tokenizer.apply_chat_template(
                    messages, add_generation_prompt=True
                )
            
            logger.debug(f"Generating response for prompt: {prompt[:100]}...")
            response = generate(
                self.model, 
                self.tokenizer, 
                prompt=prompt, 
                verbose=False,
                max_tokens=500
            )
            
            # Clean up the response - remove XML tags and thinking sections
            response = response.strip()
            
            # Remove any XML-like tags
            import re
            # Remove <think>...</think> blocks
            response = re.sub(r'<think>.*?</think>', '', response, flags=re.DOTALL)
            # Remove any remaining XML tags
            response = re.sub(r'<[^>]+>', '', response)
            # Clean up extra whitespace
            response = re.sub(r'\s+', ' ', response).strip()
            
            return response
        except Exception as e:
            logger.error(f"Failed to generate response: {e}")
            return ""
    
    def execute_command(self, command_type: str, args: str, selected_text: Optional[str] = None) -> Optional[str]:
        """Execute an assistant command and return the result."""
        if not selected_text:
            selected_text = self.get_selected_text()
            if not selected_text:
                logger.warning("No text selected for command")
                return None
        
        prompts = {
            'rewrite': f"Rewrite this text {args}:\n\n{selected_text}\n\nReturn only the rewritten text:",
            'explain': f"Explain this:\n\n{selected_text}\n\nReturn only the explanation:",
            'summarize': f"Summarize this:\n\n{selected_text}\n\nReturn only the summary:",
            'translate': f"Translate to {args}:\n\n{selected_text}\n\nReturn only the translation:",
            'fix': f"""Fix all typos and casing and punctuation and grammar in this text:

{selected_text}

Return only the corrected text, don't include a preamble:"""
        }
        
        prompt = prompts.get(command_type)
        if not prompt:
            logger.error(f"Unknown command type: {command_type}")
            return None
        
        return self.generate_response(prompt)
    
    def format_for_app_context(self, text: str, sign_off: Optional[str] = None) -> str:
        """
        Apply app-specific formatting to transcribed text.
        Returns formatted text or original if no formatting needed.
        
        Args:
            text: The transcribed text to format
            sign_off: Optional email sign-off (uses config if not provided)
        """
        # Check if we're in an email app and formatting is enabled
        from .config import is_email_formatting_enabled
        
        if not is_email_formatting_enabled():
            return text
            
        app_name = get_active_app()
        if app_name and any(mail in app_name for mail in ['Outlook', 'Mail']):
            # For email apps, apply minimal formatting and add sign-off
            if sign_off is None:
                sign_off = get_email_sign_off()
            
            # Simple formatting: ensure proper capitalization and punctuation
            formatted_text = text.strip()
            
            # Capitalize first letter if needed
            if formatted_text and not formatted_text[0].isupper():
                formatted_text = formatted_text[0].upper() + formatted_text[1:]
            
            # Add period if missing at the end
            if formatted_text and not formatted_text[-1] in '.!?':
                formatted_text += '.'
            
            # Add sign-off
            return formatted_text + "\n\n" + sign_off
        
        # No special formatting needed for other apps
        return text
    
    def process_transcription(self, text: str) -> bool:
        """
        Process transcribed text in assistant mode.
        Returns True if handled as command, False for regular dictation.
        """
        if not self.enabled:
            return False
        
        command_type, args = self.parse_command(text)
        
        if command_type:
            logger.info(f"Executing command: {command_type} with args: {args}")
            result = self.execute_command(command_type, args)
            
            if result:
                self.replace_selected_text(result)
                return True
            else:
                logger.warning("Command execution failed")
                return False
        
        return False