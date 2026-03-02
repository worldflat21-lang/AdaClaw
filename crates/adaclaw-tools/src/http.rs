use adaclaw_core::tool::{Tool, ToolResult, ToolSpec};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use reqwest::{Client, Method};
use serde_json::Value;
use std::str::FromStr;
use std::time::Duration;

// Phase 14-P1-1: import SSRF protection from adaclaw-security
use adaclaw_security::ssrf::check_ssrf_url;

pub struct HttpRequestTool {
    client: Client,
}

impl Default for HttpRequestTool {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpRequestTool {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("AdaClaw/0.1")
            .build()
            .expect("Failed to build HTTP client");
        Self { client }
    }
}

#[async_trait]
impl Tool for HttpRequestTool {
    fn name(&self) -> &str {
        "http_request"
    }

    fn description(&self) -> &str {
        "Make an HTTP request to a URL. Returns status code and response body."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to request"
                },
                "method": {
                    "type": "string",
                    "description": "HTTP method (GET, POST, PUT, DELETE, PATCH). Default: GET",
                    "enum": ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD"],
                    "default": "GET"
                },
                "headers": {
                    "type": "object",
                    "description": "Optional HTTP headers as key-value pairs",
                    "additionalProperties": { "type": "string" }
                },
                "body": {
                    "type": "string",
                    "description": "Optional request body (for POST/PUT/PATCH)"
                },
                "json": {
                    "type": "object",
                    "description": "Optional JSON body (sets Content-Type: application/json automatically)"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Request timeout in seconds (default: 30)",
                    "default": 30
                }
            },
            "required": ["url"]
        })
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters_schema(),
        }
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing 'url' argument"))?;

        // Reject non-HTTP(S) schemes for safety
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Only http:// and https:// URLs are allowed, got: {}",
                    url
                )),
            });
        }

        // Phase 14-P1-1: SSRF protection — block private/internal IP targets.
        // DNS resolution is performed before the actual HTTP request so that
        // hostname aliases that resolve to loopback/private IPs are also caught.
        if let Err(ssrf_err) = check_ssrf_url(url).await {
            tracing::warn!(url = %url, error = %ssrf_err, "http_request blocked by SSRF filter");
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(ssrf_err.to_string()),
            });
        }

        let method_str = args["method"].as_str().unwrap_or("GET").to_uppercase();
        let method = Method::from_str(&method_str).unwrap_or(Method::GET);

        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(30);

        let mut req_builder = self
            .client
            .request(method, url)
            .timeout(Duration::from_secs(timeout_secs));

        // Add custom headers
        if let Some(headers) = args["headers"].as_object() {
            for (key, val) in headers {
                if let Some(v) = val.as_str() {
                    req_builder = req_builder.header(key.as_str(), v);
                }
            }
        }

        // Body: prefer json object, fall back to raw string body
        if !args["json"].is_null() {
            req_builder = req_builder.json(&args["json"]);
        } else if let Some(body_str) = args["body"].as_str() {
            req_builder = req_builder.body(body_str.to_string());
        }

        match req_builder.send().await {
            Ok(resp) => {
                let status = resp.status();
                let status_code = status.as_u16();
                let body = resp.text().await.unwrap_or_default();

                let output = format!("HTTP {}\n{}", status_code, body);
                Ok(ToolResult {
                    success: status.is_success(),
                    output,
                    error: if status.is_success() {
                        None
                    } else {
                        Some(format!("HTTP error {}", status_code))
                    },
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Request failed: {}", e)),
            }),
        }
    }
}
