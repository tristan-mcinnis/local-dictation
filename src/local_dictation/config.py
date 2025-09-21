"""
Configuration management for Local Dictation.
Handles loading and saving settings from config files.
"""

import os
import yaml
import logging
from pathlib import Path
from typing import Dict, Any, Optional

logger = logging.getLogger(__name__)

def get_config_dir() -> Path:
    """Get the configuration directory for the application."""
    if os.name == 'posix':  # macOS/Linux
        config_dir = Path.home() / '.config' / 'local-dictation'
    else:  # Windows
        config_dir = Path.home() / 'AppData' / 'Local' / 'local-dictation'
    
    config_dir.mkdir(parents=True, exist_ok=True)
    return config_dir

def get_config_path() -> Path:
    """Get the path to the config file."""
    return get_config_dir() / 'config.yaml'

def get_default_config() -> Dict[str, Any]:
    """Get the default configuration."""
    return {
        'email': {
            'sign_off': 'Best regards,\n[Your Name]',
            'format_enabled': True
        },
        'assistant': {
            'enabled': False,
            'provider': 'mlx',
            'model': 'mlx-community/Llama-3.2-3B-Instruct-4bit',
            'openai_model': 'gpt-5-mini',
            'openai_api_key': None,
            'openai_api_key_env': 'OPENAI_API_KEY',
            'openai_base_url': None,
            'openai_organization': None,
            'result_action': 'auto',
            'copy_result_to_clipboard': True,
            'temperature': 0.2,
            'max_output_tokens': 900,
            'system_prompt': None,
            'more_info_prompt': None,
            'use_mcp': False,
            'mcp_servers': [],
            'mcp_startup_timeout': 15.0
        },
        'hotkey': {
            'chord': 'CMD,ALT',
            'debounce_ms': 50
        },
        'audio': {
            'model': 'medium.en',
            'language': 'en',
            'max_seconds': 90,
            'wake_words': []
        }
    }

def load_config() -> Dict[str, Any]:
    """
    Load configuration from file, creating default if it doesn't exist.
    
    Returns:
        Configuration dictionary
    """
    config_path = get_config_path()
    
    if not config_path.exists():
        # Create default config
        config = get_default_config()
        save_config(config)
        logger.info(f"Created default config at: {config_path}")
        logger.info("Please edit the config file to set your email sign-off")
        return config
    
    try:
        with open(config_path, 'r') as f:
            config = yaml.safe_load(f) or {}
        
        # Merge with defaults to ensure all keys exist
        default = get_default_config()
        merged = merge_configs(default, config)
        
        # Save merged config if new keys were added
        if merged != config:
            save_config(merged)
            logger.info("Updated config with new default values")
        
        return merged
        
    except Exception as e:
        logger.error(f"Failed to load config: {e}")
        logger.warning("Using default configuration")
        return get_default_config()

def save_config(config: Dict[str, Any]) -> bool:
    """
    Save configuration to file.
    
    Args:
        config: Configuration dictionary to save
    
    Returns:
        True if successful, False otherwise
    """
    config_path = get_config_path()
    
    try:
        with open(config_path, 'w') as f:
            yaml.dump(config, f, default_flow_style=False, sort_keys=False)
        return True
    except Exception as e:
        logger.error(f"Failed to save config: {e}")
        return False

def merge_configs(default: Dict[str, Any], user: Dict[str, Any]) -> Dict[str, Any]:
    """
    Recursively merge user config with default config.
    User values take precedence.
    
    Args:
        default: Default configuration
        user: User configuration
    
    Returns:
        Merged configuration
    """
    merged = default.copy()
    
    for key, value in user.items():
        if key in merged and isinstance(merged[key], dict) and isinstance(value, dict):
            merged[key] = merge_configs(merged[key], value)
        else:
            merged[key] = value
    
    return merged

def get_email_sign_off() -> str:
    """
    Get the email sign-off from config.
    
    Returns:
        Email sign-off string
    """
    config = load_config()
    return config.get('email', {}).get('sign_off', 'Best regards,\n[Your Name]')

def is_email_formatting_enabled() -> bool:
    """
    Check if email formatting is enabled.

    Returns:
        True if email formatting is enabled
    """
    config = load_config()
    return config.get('email', {}).get('format_enabled', True)

def get_assistant_settings() -> Dict[str, Any]:
    """Return assistant configuration block with defaults applied."""

    config = load_config()
    return config.get('assistant', {})
