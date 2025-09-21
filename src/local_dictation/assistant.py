"""Assistant mode implementation for transforming selected text on macOS.

This module provides a flexible assistant that can operate either with
local MLX language models or with the latest OpenAI models (GPT-5 family).
It focuses on macOS workflows where a user selects text, issues a voice
command, and receives a transformed result that can be pasted back,
copied to the clipboard, or shown in a separate window for review.
"""

import os
import platform
import re
import subprocess
import threading
import time
import logging
from dataclasses import dataclass
from typing import Any, Dict, List, Optional, Tuple

import pyperclip
from pynput.keyboard import Key, Controller

from .app_context import get_active_app
from .config import get_email_sign_off
from .mcp_client import MCPServerConfig, MCPToolClient

try:  # pragma: no cover - type hints only available when package present
    from openai.types.responses.response_output_item import McpCall as ResponseMcpCall
    from openai.types.responses.response_output_item import McpListTools as ResponseMcpListTools
    from openai.types.responses.response_output_message import ResponseOutputMessage
except Exception:  # pragma: no cover - fallback when optional types missing
    ResponseMcpCall = None
    ResponseMcpListTools = None
    ResponseOutputMessage = None

logger = logging.getLogger(__name__)

controller = Controller()


DEFAULT_SYSTEM_PROMPT = (
    "You are a macOS writing assistant. When the user asks for a "
    "transformation, respond only with the transformed text with no extra "
    "preamble."
)

DEFAULT_MORE_INFO_PROMPT = (
    "You are a macOS research assistant. Provide succinct explanations, "
    "context, and follow-up ideas in clear plain text or bullet lists."
)


def _env_or_default(name: Optional[str], fallback: Optional[str]) -> Optional[str]:
    """Helper to read an environment variable with a fallback."""

    if name:
        value = os.getenv(name)
        if value:
            return value
    return fallback


@dataclass
class AssistantSettings:
    """Configuration options for the Assistant."""

    provider: str = "mlx"
    model_name: Optional[str] = None
    openai_model: Optional[str] = None
    openai_api_key: Optional[str] = None
    openai_api_key_env: str = "OPENAI_API_KEY"
    openai_organization: Optional[str] = None
    openai_base_url: Optional[str] = None
    result_action: str = "auto"
    copy_result_to_clipboard: bool = True
    temperature: float = 0.2
    max_output_tokens: int = 900
    system_prompt: Optional[str] = None
    more_info_prompt: Optional[str] = None
    use_mcp: bool = False
    mcp_servers: Optional[List[Any]] = None
    mcp_startup_timeout: float = 15.0

    def normalized(self) -> "AssistantSettings":
        """Return a sanitized copy with normalized provider/model selections."""

        provider = (self.provider or "mlx").lower()
        model_name = self.model_name
        mcp_servers: List[MCPServerConfig] = []
        if self.mcp_servers:
            for entry in self.mcp_servers:
                if isinstance(entry, MCPServerConfig):
                    mcp_servers.append(entry)
                elif isinstance(entry, dict):
                    try:
                        mcp_servers.append(MCPServerConfig.from_mapping(entry))
                    except Exception as exc:
                        logger.error("Invalid MCP server configuration %s: %s", entry, exc)
                else:
                    logger.error("Unsupported MCP server entry: %s", entry)
        use_mcp = self.use_mcp and bool(mcp_servers)
        if provider == "openai":
            model_name = self.openai_model or model_name or "gpt-5-mini"
        else:
            model_name = model_name or "mlx-community/Llama-3.2-3B-Instruct-4bit"
        return AssistantSettings(
            provider=provider,
            model_name=model_name,
            openai_model=self.openai_model or (model_name if provider == "openai" else None),
            openai_api_key=self.openai_api_key,
            openai_api_key_env=self.openai_api_key_env or "OPENAI_API_KEY",
            openai_organization=self.openai_organization,
            openai_base_url=self.openai_base_url,
            result_action=self.result_action or "auto",
            copy_result_to_clipboard=self.copy_result_to_clipboard,
            temperature=self.temperature,
            max_output_tokens=self.max_output_tokens,
            system_prompt=self.system_prompt or DEFAULT_SYSTEM_PROMPT,
            more_info_prompt=self.more_info_prompt or DEFAULT_MORE_INFO_PROMPT,
            use_mcp=use_mcp,
            mcp_servers=mcp_servers,
            mcp_startup_timeout=self.mcp_startup_timeout,
        )


class Assistant:
    """Handles assistant mode operations for selected text on macOS."""

    def __init__(
        self,
        model_name: Optional[str] = None,
        provider: str = "mlx",
        *,
        openai_model: Optional[str] = None,
        openai_api_key: Optional[str] = None,
        openai_api_key_env: str = "OPENAI_API_KEY",
        openai_organization: Optional[str] = None,
        openai_base_url: Optional[str] = None,
        result_action: str = "auto",
        copy_result_to_clipboard: bool = True,
        temperature: float = 0.2,
        max_output_tokens: int = 900,
        system_prompt: Optional[str] = None,
        more_info_prompt: Optional[str] = None,
        use_mcp: bool = False,
        mcp_servers: Optional[List[Any]] = None,
        mcp_startup_timeout: float = 15.0,
    ):
        """Initialize the assistant with model/provider configuration."""

        settings = AssistantSettings(
            provider=provider,
            model_name=model_name,
            openai_model=openai_model,
            openai_api_key=openai_api_key,
            openai_api_key_env=openai_api_key_env,
            openai_organization=openai_organization,
            openai_base_url=openai_base_url,
            result_action=result_action,
            copy_result_to_clipboard=copy_result_to_clipboard,
            temperature=temperature,
            max_output_tokens=max_output_tokens,
            system_prompt=system_prompt,
            more_info_prompt=more_info_prompt,
            use_mcp=use_mcp,
            mcp_servers=mcp_servers,
            mcp_startup_timeout=mcp_startup_timeout,
        ).normalized()

        self.provider = settings.provider
        self.model_name = settings.model_name
        self.openai_model = settings.openai_model or self.model_name
        self.openai_api_key = settings.openai_api_key
        self.openai_api_key_env = settings.openai_api_key_env
        self.openai_organization = settings.openai_organization
        self.openai_base_url = settings.openai_base_url
        self.result_action = settings.result_action
        self.copy_result_to_clipboard = settings.copy_result_to_clipboard
        self.temperature = settings.temperature
        self.max_output_tokens = settings.max_output_tokens
        self.system_prompt = settings.system_prompt or DEFAULT_SYSTEM_PROMPT
        self.more_info_prompt = settings.more_info_prompt or DEFAULT_MORE_INFO_PROMPT
        self.use_mcp = settings.use_mcp
        self.mcp_servers = settings.mcp_servers or []
        self.mcp_startup_timeout = settings.mcp_startup_timeout

        self.model = None
        self.tokenizer = None
        self.openai_client = None
        self.mcp_client: Optional[MCPToolClient] = None
        self.enabled = False

        if self.provider == "mlx":
            # Normalize MLX community model names
            if "/" not in self.model_name and not self.model_name.startswith("mlx-community/"):
                self.model_name = f"mlx-community/{self.model_name}"

    def _ensure_mcp_client(self) -> Optional[MCPToolClient]:
        """Initialize MCP tooling when requested."""

        if not self.use_mcp or not self.mcp_servers:
            return None

        if self.mcp_client:
            return self.mcp_client

        try:
            self.mcp_client = MCPToolClient(self.mcp_servers, startup_timeout=self.mcp_startup_timeout)
            if not self.mcp_client.ready():
                logger.warning("MCP tooling not ready")
            return self.mcp_client
        except Exception as exc:  # pragma: no cover - depends on external tooling
            logger.error("Unable to initialize MCP tooling: %s", exc)
            self.use_mcp = False
            self.mcp_client = None
            return None

    def load_model(self):
        """Lazy load the selected provider's resources when first needed."""

        if self.provider == "openai":
            if self.openai_client is not None:
                return True

            try:
                from openai import OpenAI
            except ImportError:
                logger.error("openai package not installed. Install with: uv add openai")
                self.enabled = False
                return False

            api_key = _env_or_default(self.openai_api_key_env, self.openai_api_key)
            if not api_key:
                logger.error(
                    "OpenAI API key not found. Set the %s environment variable or provide a key via settings.",
                    self.openai_api_key_env,
                )
                self.enabled = False
                return False

            client_kwargs: Dict[str, str] = {"api_key": api_key}
            if self.openai_organization:
                client_kwargs["organization"] = self.openai_organization
            if self.openai_base_url:
                client_kwargs["base_url"] = self.openai_base_url

            try:
                self.openai_client = OpenAI(**client_kwargs)
                logger.info("Initialized OpenAI client for model %s", self.model_name)
            except Exception as exc:  # pragma: no cover - depends on runtime env
                logger.error("Failed to initialize OpenAI client: %s", exc)
                self.enabled = False
                return False

            return True

        if self.model is None:
            try:
                from mlx_lm import load

                logger.info("Loading MLX model: %s", self.model_name)
                logger.info("Note: First-time model download may take a few minutes...")
                self.model, self.tokenizer = load(self.model_name)
                logger.info("Model loaded successfully")
            except ImportError:
                logger.error("mlx-lm not installed. Install with: uv add mlx-lm")
                self.enabled = False
                return False
            except Exception as e:  # pragma: no cover - hardware dependent
                error_msg = str(e)
                logger.error("Failed to load model: %s", e)

                if any(keyword in error_msg.lower() for keyword in ("metal", "gpu", "memory")):
                    logger.error(
                        "GPU/Metal resources may be busy. If Ollama is running, consider stopping it."
                    )
                    logger.error("Run 'ollama stop' or restart this app after stopping other GPU-intensive apps.")
                else:
                    logger.error("Try running again - the model may still be downloading.")
                    logger.error("Or check your internet connection and try a different model.")

                self.enabled = False
                return False
        return True

    def enable(self):
        """Enable assistant mode."""
        if self.load_model():
            if self.use_mcp:
                self._ensure_mcp_client()
            self.enabled = True
            logger.info("Assistant mode enabled")
        else:
            logger.warning("Assistant mode disabled due to model loading failure")
    
    def disable(self):
        """Disable assistant mode."""
        self.enabled = False
        logger.info("Assistant mode disabled")
        if self.mcp_client:
            try:
                self.mcp_client.shutdown()
            except Exception:  # pragma: no cover - best-effort cleanup
                pass
            self.mcp_client = None
    
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
            ],
            'research': [
                r'^research (?:this|that)?(.*)$',
                r'^find (?:more )?information(?: about)?(.*)$',
                r'^give me more (?:details|information)(?: about)?(.*)$',
                r'^what else should i know(?: about)?(.*)$'
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
            # Save current clipboard first to restore later
            old_clipboard = ""
            try:
                old_clipboard = pyperclip.paste()
            except:
                pass

            # Copy selection to clipboard (just like the reference code)
            with controller.pressed(Key.cmd):
                controller.tap("c")

            # Get the clipboard string with a longer delay to ensure it's updated
            time.sleep(0.15)
            text = pyperclip.paste()

            # Restore original clipboard content if we got text
            if text and text != old_clipboard:
                logger.debug(f"Text copied from selection: {text[:50]}...")
                # Defer restoring clipboard to prevent interference
                threading.Timer(0.5, lambda: pyperclip.copy(old_clipboard)).start()
                return text
            else:
                logger.warning("No text in clipboard after copy")
                # Restore original clipboard immediately if no new text
                if old_clipboard:
                    pyperclip.copy(old_clipboard)
                return None
        except Exception as e:
            logger.error(f"Failed to get selected text: {e}")
            return None
    
    def replace_selected_text(self, new_text: str, preserve_clipboard: bool = False):
        """Replace selected text with new text."""

        try:
            old_clipboard = None
            if preserve_clipboard:
                try:
                    old_clipboard = pyperclip.paste()
                except Exception:  # pragma: no cover - depends on clipboard
                    old_clipboard = None

            pyperclip.copy(new_text)
            time.sleep(0.1)

            with controller.pressed(Key.cmd):
                controller.tap("v")

            time.sleep(0.05)

            if preserve_clipboard and old_clipboard is not None:
                threading.Timer(0.5, lambda: pyperclip.copy(old_clipboard)).start()

            logger.debug("Replaced text with corrected version")
        except Exception as e:  # pragma: no cover - clipboard interaction
            logger.error("Failed to replace text: %s", e)

    def copy_to_clipboard(self, new_text: str):
        """Copy the assistant result to the clipboard."""

        try:
            pyperclip.copy(new_text)
            logger.debug("Copied assistant result to clipboard")
        except Exception as exc:  # pragma: no cover - clipboard interaction
            logger.error("Failed to copy result to clipboard: %s", exc)

    def show_in_textedit(self, new_text: str) -> bool:
        """Display the assistant result in a new TextEdit window on macOS."""

        if platform.system() != "Darwin":  # pragma: no cover - macOS specific
            logger.warning("TextEdit presentation is only available on macOS")
            return False

        script_lines = [
            "on run argv",
            "set theText to item 1 of argv",
            "tell application \"TextEdit\"",
            "activate",
            "set newDoc to make new document",
            "set text of newDoc to theText",
            "end tell",
            "end run",
        ]

        try:
            subprocess.run(
                ["osascript", *sum((['-e', line] for line in script_lines), []), new_text],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            logger.debug("Opened assistant result in TextEdit")
            return True
        except FileNotFoundError:
            logger.error("osascript not found. Unable to present result in TextEdit.")
        except subprocess.CalledProcessError as exc:
            logger.error("Failed to display result in TextEdit: %s", exc.stderr.decode("utf-8", "ignore"))
        return False

    def generate_response(self, prompt: str, command_type: Optional[str] = None) -> str:
        """Generate response using the configured language model."""

        if self.provider == "openai":
            return self._generate_openai_response(prompt, command_type)

        if not self.model:
            if not self.load_model():
                return ""

        try:
            from mlx_lm import generate

            if self.tokenizer.chat_template is not None:
                messages = [{"role": "user", "content": prompt}]
                prompt = self.tokenizer.apply_chat_template(messages, add_generation_prompt=True)

            logger.debug("Generating response with MLX model for prompt: %s", prompt[:100])
            response = generate(
                self.model,
                self.tokenizer,
                prompt=prompt,
                verbose=False,
                max_tokens=self.max_output_tokens,
            )

            response = response.strip()
            response = re.sub(r'<think>.*?</think>', '', response, flags=re.DOTALL)
            response = re.sub(r'<[^>]+>', '', response)
            response = re.sub(r'\s+', ' ', response).strip()

            return response
        except Exception as e:  # pragma: no cover - depends on MLX runtime
            logger.error("Failed to generate response: %s", e)
            return ""

    def _generate_openai_response(self, prompt: str, command_type: Optional[str]) -> str:
        """Generate a response from the OpenAI Responses API."""

        if not self.openai_client:
            if not self.load_model():
                return ""

        system_prompt = self.system_prompt
        if command_type in {"explain", "summarize", "research"}:
            system_prompt = self.more_info_prompt or self.system_prompt

        try:
            tool_client = self._ensure_mcp_client() if self.use_mcp else None
            tools_param = tool_client.openai_tool_params() if tool_client else []

            request_kwargs = {
                "model": self.model_name,
                "input": [{"role": "user", "content": prompt}],
                "instructions": system_prompt,
                "max_output_tokens": self.max_output_tokens,
                "temperature": self.temperature,
            }
            if tools_param:
                request_kwargs["tools"] = tools_param

            response = self.openai_client.responses.create(**request_kwargs)
            return self._resolve_openai_response(response, tool_client).strip()
        except Exception as exc:  # pragma: no cover - network interaction
            logger.error("OpenAI response generation failed: %s", exc)
        return ""

    def _resolve_openai_response(self, response: Any, tool_client: Optional[MCPToolClient]) -> str:
        """Handle OpenAI Responses output, executing MCP tools if requested."""

        text = self._extract_openai_text(response)
        if not tool_client:
            return text

        pending_inputs = self._build_tool_feedback(response, tool_client)
        previous_id = getattr(response, "id", None)

        while pending_inputs and previous_id:
            try:
                response = self.openai_client.responses.create(
                    model=self.model_name,
                    previous_response_id=previous_id,
                    input=pending_inputs,
                    max_output_tokens=self.max_output_tokens,
                )
            except Exception as exc:  # pragma: no cover - network interaction
                logger.error("Failed to submit MCP tool outputs: %s", exc)
                break

            new_text = self._extract_openai_text(response)
            if new_text:
                text = new_text

            pending_inputs = self._build_tool_feedback(response, tool_client)
            previous_id = getattr(response, "id", None)

        return text

    def _extract_openai_text(self, response: Any) -> str:
        if hasattr(response, "output_text") and response.output_text:
            return str(response.output_text).strip()

        output = getattr(response, "output", None)
        if not output:
            return ""

        parts: List[str] = []
        for item in output:
            item_type = getattr(item, "type", None)
            if item_type == "message" or (ResponseOutputMessage and isinstance(item, ResponseOutputMessage)):
                for content in getattr(item, "content", []) or []:
                    text_value = getattr(content, "text", None)
                    if text_value:
                        parts.append(str(text_value))
        return "\n".join(part for part in parts if part).strip()

    def _build_tool_feedback(self, response: Any, tool_client: MCPToolClient) -> List[Dict[str, Any]]:
        output = getattr(response, "output", None)
        if not output:
            return []

        feedback: List[Dict[str, Any]] = []
        for item in output:
            item_type = getattr(item, "type", None)
            if item_type == "mcp_list_tools" or (ResponseMcpListTools and isinstance(item, ResponseMcpListTools)):
                server_label = getattr(item, "server_label", "")
                tool_list = tool_client.describe_tools(server_label)
                feedback.append({
                    "type": "mcp_list_tools",
                    "id": getattr(item, "id", ""),
                    "server_label": server_label,
                    "tools": tool_list,
                })
            elif item_type == "mcp_call" or (ResponseMcpCall and isinstance(item, ResponseMcpCall)):
                server_label = getattr(item, "server_label", "")
                tool_name = getattr(item, "name", "")
                arguments = getattr(item, "arguments", "")
                output_text, error_text = tool_client.call_tool(server_label, tool_name, arguments)
                payload: Dict[str, Any] = {
                    "type": "mcp_call",
                    "id": getattr(item, "id", ""),
                    "server_label": server_label,
                    "name": tool_name,
                    "arguments": arguments,
                }
                if output_text:
                    payload["output"] = output_text
                if error_text:
                    payload["error"] = error_text
                feedback.append(payload)

        return feedback

    def execute_command(
        self, command_type: str, args: str, selected_text: Optional[str] = None
    ) -> Optional[str]:
        """Execute an assistant command and return the result."""
        if not selected_text:
            selected_text = self.get_selected_text()
            if not selected_text:
                logger.warning("No text selected for command")
                return None

        prompts = {
            'rewrite': f"Rewrite this text {args}:\n\n{selected_text}\n\nReturn only the rewritten text:",
            'explain': f"""Provide a detailed explanation of this text. Break down the concepts, explain any technical terms, and provide context where helpful:

{selected_text}

Return only the explanation:""",
            'summarize': f"""Provide a comprehensive summary of this text. Include the main points, key details, and important conclusions. Make it thorough but concise:

{selected_text}

Return only the summary:""",
            'translate': f"Translate to {args}:\n\n{selected_text}\n\nReturn only the translation:",
            'fix': f"""Fix all typos and casing and punctuation and grammar in this text:

{selected_text}

Return only the corrected text, don't include a preamble:""",
            'research': f"""You are a helpful macOS research assistant. Provide additional context, important background facts, relevant examples, and suggested follow-up questions that would help a writer expand on this text:

{selected_text}

Return the information as plain text or Markdown bullet points without any preamble:"""
        }
        
        prompt = prompts.get(command_type)
        if not prompt:
            logger.error(f"Unknown command type: {command_type}")
            return None
        
        return self.generate_response(prompt, command_type)

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
            logger.info("Executing command: %s with args: %s", command_type, args)
            result = self.execute_command(command_type, args)

            if result:
                self.deliver_result(result, command_type)
                return True

            logger.warning("Command execution failed")
            return False

        return False

    def deliver_result(self, result: str, command_type: Optional[str] = None):
        """Deliver the assistant result based on the configured action."""

        action = self.result_action
        if action == "auto":
            if command_type in {"explain", "summarize", "research"}:
                action_to_use = "show_textedit"
            else:
                action_to_use = "replace_selection"
        else:
            action_to_use = action

        if action_to_use == "replace_selection":
            preserve = not self.copy_result_to_clipboard
            self.replace_selected_text(result, preserve_clipboard=preserve)
            if preserve and self.copy_result_to_clipboard:
                self.copy_to_clipboard(result)
            elif self.copy_result_to_clipboard:
                # Result already on clipboard due to paste; nothing else required
                pass
        elif action_to_use == "copy_to_clipboard":
            self.copy_to_clipboard(result)
        elif action_to_use == "show_textedit":
            shown = self.show_in_textedit(result)
            if self.copy_result_to_clipboard or not shown:
                self.copy_to_clipboard(result)
        else:
            logger.warning("Unknown result action '%s', defaulting to clipboard copy", action_to_use)
            self.copy_to_clipboard(result)
