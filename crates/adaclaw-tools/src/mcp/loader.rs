//! MCP Server 自动发现与注册
//!
//! 启动时读取 `config.tools.mcp_servers`，对每个配置的 MCP Server：
//! 1. 建立连接（Stdio 或 HTTP）
//! 2. 握手并获取工具清单
//! 3. 将所有 `McpTool` 注册到调用者提供的 Vec<Box<dyn Tool>>

use super::{http::HttpMcpClient, stdio::StdioMcpClient, McpTool, McpTransport};
use adaclaw_core::tool::Tool;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};

/// MCP Server 配置（与 Claude Desktop / nanobot 格式兼容）
///
/// ```toml
/// # Stdio transport（本地进程）
/// [tools.mcp_servers.filesystem]
/// command = "npx"
/// args    = ["-y", "@modelcontextprotocol/server-filesystem", "/workspace"]
///
/// # HTTP transport（远程服务器）
/// [tools.mcp_servers.remote]
/// url     = "https://example.com/mcp/"
/// headers = { Authorization = "Bearer xxx" }
/// tool_timeout = 30
/// ```
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    /// Stdio transport：启动本地进程（npx / uvx / 可执行文件）
    Stdio {
        /// 可执行命令（如 "npx", "uvx", "/usr/local/bin/my-mcp"）
        command: String,
        /// 命令行参数
        #[serde(default)]
        args: Vec<String>,
        /// 可选环境变量
        #[serde(default)]
        env: Option<HashMap<String, String>>,
        /// 工具调用超时（秒，默认 30）
        #[serde(default)]
        tool_timeout: Option<u64>,
    },
    /// HTTP transport：连接远程 MCP Server
    Http {
        /// 服务器 URL
        url: String,
        /// 自定义 HTTP 请求头（如 Authorization）
        #[serde(default)]
        headers: Option<HashMap<String, String>>,
        /// 请求超时（秒，默认 30）
        #[serde(default)]
        tool_timeout: Option<u64>,
    },
}

/// MCP 加载器：读取配置并批量初始化所有 MCP Server
pub struct McpLoader;

impl McpLoader {
    /// 加载所有配置中的 MCP Server，返回 `Vec<McpTool>`（可克隆）
    ///
    /// 如果某个 MCP Server 无法连接，打印警告并跳过（不中断启动）。
    pub async fn load_all_clonable(
        mcp_servers: &HashMap<String, McpServerConfig>,
    ) -> Vec<super::McpTool> {
        let mut tools: Vec<super::McpTool> = Vec::new();

        for (server_name, config) in mcp_servers {
            match Self::load_server_clonable(server_name, config).await {
                Ok(server_tools) => {
                    info!(
                        server = %server_name,
                        tools = server_tools.len(),
                        "MCP server loaded"
                    );
                    tools.extend(server_tools);
                }
                Err(e) => {
                    warn!(
                        server = %server_name,
                        error = %e,
                        "Failed to load MCP server, skipping"
                    );
                }
            }
        }

        tools
    }

    /// 加载所有配置中的 MCP Server，返回包装为 `Box<dyn Tool>` 的工具列表
    pub async fn load_all(
        mcp_servers: &HashMap<String, McpServerConfig>,
    ) -> Vec<Box<dyn Tool>> {
        Self::load_all_clonable(mcp_servers)
            .await
            .into_iter()
            .map(|t| Box::new(t) as Box<dyn Tool>)
            .collect()
    }

    /// 连接单个 MCP Server 并返回 McpTool 列表（可克隆）
    async fn load_server_clonable(
        server_name: &str,
        config: &McpServerConfig,
    ) -> Result<Vec<super::McpTool>> {
        match config {
            McpServerConfig::Stdio {
                command,
                args,
                env,
                tool_timeout,
            } => {
                let timeout = tool_timeout.unwrap_or(30);
                let (transport, tool_descs) = StdioMcpClient::connect(
                    server_name,
                    command,
                    args.clone(),
                    env.clone(),
                )
                .await?;

                let transport_arc: Arc<dyn McpTransport> = transport;
                let tools = Self::build_clonable_tools(server_name, tool_descs, transport_arc, timeout);
                Ok(tools)
            }

            McpServerConfig::Http {
                url,
                headers,
                tool_timeout,
            } => {
                let timeout = tool_timeout.unwrap_or(30);
                let (transport, tool_descs) = HttpMcpClient::connect(
                    server_name,
                    url,
                    headers.clone(),
                    Some(timeout),
                )
                .await?;

                let transport_arc: Arc<dyn McpTransport> = transport;
                let tools = Self::build_clonable_tools(server_name, tool_descs, transport_arc, timeout);
                Ok(tools)
            }
        }
    }

    /// 将 McpToolDescription 列表包装为 McpTool 列表
    fn build_clonable_tools(
        server_name: &str,
        tool_descs: Vec<super::McpToolDescription>,
        transport: Arc<dyn McpTransport>,
        timeout_secs: u64,
    ) -> Vec<super::McpTool> {
        tool_descs
            .iter()
            .map(|desc| {
                let tool = McpTool::new(server_name, desc, Arc::clone(&transport), timeout_secs);
                info!(
                    server = %server_name,
                    tool = %desc.name,
                    description = ?desc.description,
                    "MCP tool registered"
                );
                tool
            })
            .collect()
    }

    /// 将 McpToolDescription 列表包装为 Box<dyn Tool> 列表（旧方法，内部使用）
    #[allow(dead_code)]
    fn build_tools(
        server_name: &str,
        tool_descs: Vec<super::McpToolDescription>,
        transport: Arc<dyn McpTransport>,
        timeout_secs: u64,
    ) -> Vec<Box<dyn Tool>> {
        Self::build_clonable_tools(server_name, tool_descs, transport, timeout_secs)
            .into_iter()
            .map(|t| Box::new(t) as Box<dyn Tool>)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stdio_config_deserializes() {
        let toml = r#"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/workspace"]
"#;
        let cfg: McpServerConfig = toml::from_str(toml).unwrap();
        assert!(matches!(cfg, McpServerConfig::Stdio { .. }));
        if let McpServerConfig::Stdio { command, args, .. } = cfg {
            assert_eq!(command, "npx");
            assert_eq!(args.len(), 3);
        }
    }

    #[test]
    fn test_http_config_deserializes() {
        let toml = r#"
url = "https://example.com/mcp/"
tool_timeout = 60
"#;
        let cfg: McpServerConfig = toml::from_str(toml).unwrap();
        assert!(matches!(cfg, McpServerConfig::Http { .. }));
        if let McpServerConfig::Http { url, tool_timeout, .. } = cfg {
            assert_eq!(url, "https://example.com/mcp/");
            assert_eq!(tool_timeout, Some(60));
        }
    }
}
