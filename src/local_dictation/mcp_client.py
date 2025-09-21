"""Utility helpers for managing Model Context Protocol tool servers."""

from __future__ import annotations

import asyncio
import json
import logging
import threading
from dataclasses import dataclass, field
from typing import Any, Dict, Iterable, List, Mapping, Optional, Sequence, Tuple

from mcp import types
from mcp.client.session import ClientSession
from mcp.client.session_group import ClientSessionGroup
from mcp.client.stdio import StdioServerParameters

logger = logging.getLogger(__name__)


@dataclass
class MCPServerConfig:
    """Normalized configuration for an MCP stdio server."""

    label: str
    command: str
    args: List[str] = field(default_factory=list)
    env: Optional[Dict[str, str]] = None
    cwd: Optional[str] = None
    allowed_tools: Optional[List[str]] = None
    description: Optional[str] = None

    @classmethod
    def from_mapping(cls, value: Mapping[str, Any]) -> "MCPServerConfig":
        label = str(value.get("label") or value.get("name") or "server")
        command = value.get("command")
        if not command:
            raise ValueError("MCP server configuration requires a 'command'")

        if isinstance(command, str):
            args: List[str] = value.get("args") or []
            cmd = command
        else:
            # When command is provided as list, treat first entry as command and
            # remainder as default args.
            command_list = list(command)
            if not command_list:
                raise ValueError("MCP server configuration command list cannot be empty")
            cmd = command_list[0]
            args = command_list[1:]

        extra_args = value.get("args")
        if isinstance(extra_args, Iterable) and not isinstance(extra_args, (str, bytes)):
            args = list(args) + [str(x) for x in extra_args]

        allowed_tools = value.get("allowed_tools")
        if allowed_tools is not None:
            if isinstance(allowed_tools, str):
                allowed = [tool.strip() for tool in allowed_tools.split(",") if tool.strip()]
            else:
                allowed = [str(tool) for tool in allowed_tools]
        else:
            allowed = None

        env_value = value.get("env")
        env = {str(k): str(v) for k, v in (env_value or {}).items()} if isinstance(env_value, Mapping) else None

        cwd_value = value.get("cwd")
        cwd = str(cwd_value) if cwd_value else None

        description = value.get("description")
        if description is not None:
            description = str(description)

        return cls(
            label=label,
            command=str(cmd),
            args=list(args),
            env=env,
            cwd=cwd,
            allowed_tools=allowed,
            description=description,
        )


class MCPToolClient:
    """Manage MCP tool servers and expose them to the assistant."""

    def __init__(
        self,
        servers: Sequence[MCPServerConfig],
        *,
        startup_timeout: float = 15.0,
    ) -> None:
        self._servers = list(servers)
        self._startup_timeout = max(0.0, startup_timeout)
        self._loop = asyncio.new_event_loop()
        self._thread = threading.Thread(target=self._loop.run_forever, daemon=True)
        self._thread.start()

        self._group: Optional[ClientSessionGroup] = None
        self._sessions: Dict[str, Tuple[types.Implementation, ClientSession]] = {}
        self._tools_by_server: Dict[str, Dict[str, types.Tool]] = {}
        self._allowed_tools: Dict[str, List[str]] = {}

        self._ready = threading.Event()
        self._closed = False

        init_future = asyncio.run_coroutine_threadsafe(self._async_initialize(), self._loop)
        try:
            init_future.result(timeout=self._startup_timeout if self._servers else None)
        except Exception as exc:  # pragma: no cover - depends on external tooling
            logger.error("Failed to initialize MCP tool servers: %s", exc)
            self._ready.set()
            raise

    async def _async_initialize(self) -> None:
        if not self._servers:
            self._ready.set()
            return

        self._group = ClientSessionGroup()
        await self._group.__aenter__()

        for server in self._servers:
            try:
                params = StdioServerParameters(
                    command=server.command,
                    args=list(server.args),
                    env=server.env,
                    cwd=server.cwd,
                )
                server_info, session = await self._group._establish_session(params)
                await self._group.connect_with_session(server_info, session)

                label = server.label or server_info.name
                if label in self._sessions:
                    # Disambiguate duplicate labels
                    suffix = 2
                    while f"{label}-{suffix}" in self._sessions:
                        suffix += 1
                    label = f"{label}-{suffix}"

                self._sessions[label] = (server_info, session)

                tools_result = await session.list_tools()
                tool_map: Dict[str, types.Tool] = {tool.name: tool for tool in tools_result.tools}
                self._tools_by_server[label] = tool_map
                if server.allowed_tools:
                    allowed = [tool for tool in server.allowed_tools if tool in tool_map]
                else:
                    allowed = list(tool_map.keys())
                self._allowed_tools[label] = allowed

                logger.info("Connected MCP server '%s' (%s)", label, server.command)
            except Exception as exc:  # pragma: no cover - requires external servers
                logger.error("Failed to connect MCP server %s: %s", server.label, exc)

        self._ready.set()

    def ready(self) -> bool:
        return self._ready.is_set()

    def openai_tool_params(self) -> List[Dict[str, Any]]:
        if not self.ready():
            return []

        params: List[Dict[str, Any]] = []
        for label, tools in self._allowed_tools.items():
            if not tools:
                continue
            params.append({
                "type": "mcp",
                "server_label": label,
                "allowed_tools": tools,
            })
        return params

    def describe_tools(self, server_label: str) -> List[Dict[str, Any]]:
        tools = self._tools_by_server.get(server_label, {})
        allowed = set(self._allowed_tools.get(server_label, []))

        descriptions: List[Dict[str, Any]] = []
        for name, tool in tools.items():
            if allowed and name not in allowed:
                continue

            annotations: Optional[Dict[str, Any]] = None
            if tool.annotations is not None:
                if hasattr(tool.annotations, "model_dump"):
                    annotations = tool.annotations.model_dump()
                elif isinstance(tool.annotations, Mapping):
                    annotations = dict(tool.annotations)
                else:
                    annotations = str(tool.annotations)

            descriptions.append({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.inputSchema,
                "annotations": annotations,
            })
        return descriptions

    def call_tool(
        self,
        server_label: str,
        tool_name: str,
        arguments_json: Any,
    ) -> Tuple[str, Optional[str]]:
        if not self.ready():
            return "", "MCP tooling not ready"

        info_session = self._sessions.get(server_label)
        if not info_session:
            return "", f"Unknown MCP server: {server_label}"
        _, session = info_session

        allowed = set(self._allowed_tools.get(server_label, []))
        if allowed and tool_name not in allowed:
            return "", f"Tool '{tool_name}' is not permitted"

        tools = self._tools_by_server.get(server_label, {})
        if tools and tool_name not in tools:
            return "", f"Tool '{tool_name}' not found on server {server_label}"

        try:
            if isinstance(arguments_json, Mapping):
                arguments = dict(arguments_json)
            elif isinstance(arguments_json, str):
                arguments = json.loads(arguments_json) if arguments_json else {}
            elif arguments_json:
                raise ValueError("Tool arguments must be a JSON object or mapping")
            else:
                arguments = {}
            if not isinstance(arguments, dict):
                raise ValueError("Tool arguments must decode to a JSON object")
        except Exception as exc:
            return "", f"Invalid tool arguments: {exc}"

        async def _call() -> types.CallToolResult:
            return await session.call_tool(tool_name, arguments)

        try:
            result = asyncio.run_coroutine_threadsafe(_call(), self._loop).result(timeout=self._startup_timeout or None)
        except Exception as exc:  # pragma: no cover - depends on server runtime
            return "", f"Tool call failed: {exc}"

        text_parts: List[str] = []
        for content in result.content:
            if getattr(content, "type", None) == "text":
                text_parts.append(getattr(content, "text", ""))
            elif getattr(content, "type", None) == "resource":
                resource = getattr(content, "uri", None) or getattr(content, "text", "")
                if resource:
                    text_parts.append(f"[resource] {resource}")
            elif getattr(content, "type", None) == "image":
                text_parts.append("[image content]")
            elif getattr(content, "type", None) == "audio":
                text_parts.append("[audio content]")

        if result.structuredContent:
            try:
                structured_text = json.dumps(result.structuredContent, indent=2)
                text_parts.append(structured_text)
            except Exception:
                # Fallback to repr if JSON encoding fails
                text_parts.append(str(result.structuredContent))

        output_text = "\n".join(part for part in text_parts if part).strip()
        error_text = "Tool reported an error" if result.isError else None
        return output_text, error_text

    def shutdown(self) -> None:
        if self._closed:
            return
        self._closed = True

        async def _close() -> None:
            if self._group:
                await self._group.__aexit__(None, None, None)

        try:
            asyncio.run_coroutine_threadsafe(_close(), self._loop).result(timeout=5)
        except Exception:  # pragma: no cover - cleanup best effort
            pass

        self._loop.call_soon_threadsafe(self._loop.stop)
        self._thread.join(timeout=5)


def parse_mcp_server_strings(values: Sequence[str]) -> List[MCPServerConfig]:
    """Parse CLI-provided MCP server specifications."""

    import shlex

    configs: List[MCPServerConfig] = []
    for value in values:
        if "=" not in value:
            raise ValueError("MCP server specification must include label=command")
        label, command = value.split("=", 1)
        label = label.strip()
        if not label:
            raise ValueError("MCP server label cannot be empty")

        args = shlex.split(command.strip())
        if not args:
            raise ValueError("MCP server command cannot be empty")

        configs.append(MCPServerConfig(label=label, command=args[0], args=args[1:]))
    return configs

