# MCP 工具集成指南

AdaClaw 内置 Model Context Protocol (MCP) 客户端，可将任意外部 MCP Server 的工具**透明接入**，与原生工具（shell/file/http）完全同等对待。

## 概述

MCP（Model Context Protocol）是 Anthropic 定义的开放协议，允许 AI 模型通过统一接口调用外部工具。AdaClaw 作为 MCP **客户端**，支持：

- **Stdio transport**：启动本地进程（npx / uvx / 可执行文件），通过 stdin/stdout 通信
- **HTTP transport**：连接远程 MCP Server（HTTP POST JSON-RPC）

## 与 Claude Desktop 配置兼容

AdaClaw 的 MCP 配置格式与 Claude Desktop 完全兼容，可直接复用现有配置。

## 快速开始

### 1. Filesystem MCP Server（本地文件系统）

```toml
[tools.mcp_servers.filesystem]
command = "npx"
args    = ["-y", "@modelcontextprotocol/server-filesystem", "/workspace"]
```

需要 Node.js / npm：
```bash
npm install -g @modelcontextprotocol/server-filesystem
```

启动后，Agent 可使用 `read_file`、`write_file`、`list_directory` 等工具。

### 2. 远程 MCP Server（HTTP）

```toml
[tools.mcp_servers.my-service]
url          = "https://mcp.example.com/api/"
tool_timeout = 30

[tools.mcp_servers.my-secure-service]
url     = "https://secure-mcp.example.com/"
headers = { Authorization = "Bearer your-token-here" }
```

### 3. 本地可执行文件

```toml
[tools.mcp_servers.my-tool]
command = "/usr/local/bin/my-mcp-server"
args    = ["--config", "/etc/my-config.json"]
env     = { MY_API_KEY = "xxx" }
```

## 完整配置示例

```toml
# config.toml

# ── MCP Servers ────────────────────────────────────────────────────────────────

# 文件系统工具（本地进程）
[tools.mcp_servers.filesystem]
command = "npx"
args    = ["-y", "@modelcontextprotocol/server-filesystem", "/workspace"]

# Python 工具包（uvx）
[tools.mcp_servers.python-tools]
command = "uvx"
args    = ["mcp-server-python-tools"]

# 远程工具服务
[tools.mcp_servers.remote]
url          = "https://api.example.com/mcp/"
headers      = { Authorization = "Bearer sk-..." }
tool_timeout = 60

# Git 工具（带环境变量）
[tools.mcp_servers.git]
command = "npx"
args    = ["-y", "@modelcontextprotocol/server-git"]
env     = { GIT_TOKEN = "ghp_..." }
```

## 配置字段说明

### Stdio Transport

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `command` | string | ✅ | 可执行命令（如 `npx`, `uvx`, `/path/to/binary`） |
| `args` | string[] | — | 命令行参数 |
| `env` | object | — | 额外环境变量 |
| `tool_timeout` | integer | — | 工具调用超时（秒，默认 30） |

### HTTP Transport

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `url` | string | ✅ | MCP Server URL（以 `/` 结尾） |
| `headers` | object | — | 自定义 HTTP 请求头 |
| `tool_timeout` | integer | — | 超时（秒，默认 30） |

## 工作原理

1. **启动时自动发现**：daemon 启动时自动连接所有配置的 MCP Server
2. **握手**：发送 `initialize` 请求，获取服务器能力
3. **工具清单**：调用 `tools/list` 获取所有可用工具
4. **透明注入**：将 MCP 工具包装为原生 `Tool` 实现，注入每个 Agent 的工具列表
5. **调用代理**：Agent 调用 MCP 工具时，自动转发 `tools/call` 请求到对应 Server

```
Agent 调用 "filesystem:read_file"
    ↓
McpTool.execute() → McpTransport.call_tool()
    ↓
JSON-RPC: {"method": "tools/call", "params": {"name": "read_file", "arguments": {...}}}
    ↓
MCP Server 返回结果
    ↓
Agent 收到工具结果
```

## 故障处理

### Stdio 进程崩溃自动重启

Stdio transport 会在进程崩溃时自动尝试重启（最多 3 次）：

```
MCP server 'filesystem' process died, restarting... (restart_count=1)
```

超过重试次数后，该工具调用返回错误，不影响其他工具和 Agent 运行。

### 连接失败跳过

启动时某个 MCP Server 无法连接，打印警告并跳过：

```
WARN Failed to load MCP server, skipping  server="filesystem" error="Failed to spawn..."
```

## 常用 MCP Servers

| Server | 安装命令 | 用途 |
|--------|----------|------|
| `@modelcontextprotocol/server-filesystem` | `npx -y ...` | 文件读写 |
| `@modelcontextprotocol/server-github` | `npx -y ...` | GitHub API |
| `@modelcontextprotocol/server-sqlite` | `npx -y ...` | SQLite 查询 |
| `@modelcontextprotocol/server-brave-search` | `npx -y ...` | 网页搜索 |
| `mcp-server-fetch` | `uvx mcp-server-fetch` | HTTP 请求 |

完整列表见：[MCP Servers 官方目录](https://modelcontextprotocol.io/examples)
