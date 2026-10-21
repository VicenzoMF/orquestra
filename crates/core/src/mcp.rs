use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::BrowserEndpoint;

/// Servidor MCP stdio a registrar num agente.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerSpec {
    /// Nome do servidor no agente; as ferramentas aparecem como
    /// `mcp__<nome>__<ferramenta>` no Claude Code.
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Modelo de MCP de navegador na config. `args` aceita os placeholders
/// `{cdp_url}` (http), `{cdp_port}` e `{ws_url}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerTemplate {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

impl McpServerTemplate {
    pub fn playwright() -> Self {
        Self {
            name: "browser".to_owned(),
            command: "npx".to_owned(),
            args: [
                "-y",
                "@playwright/mcp@latest",
                "--cdp-endpoint",
                "{cdp_url}",
            ]
            .map(str::to_owned)
            .to_vec(),
        }
    }

    pub fn chrome_devtools() -> Self {
        Self {
            name: "browser".to_owned(),
            command: "npx".to_owned(),
            args: [
                "-y",
                "chrome-devtools-mcp@latest",
                "--browserUrl",
                "{cdp_url}",
            ]
            .map(str::to_owned)
            .to_vec(),
        }
    }

    /// Spec com os placeholders preenchidos para um navegador.
    pub fn render(&self, endpoint: &BrowserEndpoint) -> McpServerSpec {
        let http = endpoint.http_url();
        let port = endpoint.port.to_string();
        McpServerSpec {
            name: self.name.clone(),
            command: self.command.clone(),
            args: self
                .args
                .iter()
                .map(|a| {
                    a.replace("{cdp_url}", &http)
                        .replace("{cdp_port}", &port)
                        .replace("{ws_url}", &endpoint.ws_url)
                })
                .collect(),
            env: BTreeMap::new(),
        }
    }
}

/// O que o gateway entrega para um orquestrador subir já conectado.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrchestratorGrant {
    /// O servidor MCP `orquestra` (normalmente `<este binário> mcp`).
    pub mcp: McpServerSpec,
    /// Credencial e endereço do gateway (`ORQUESTRA_SOCKET`, `ORQUESTRA_TOKEN`).
    pub env: BTreeMap<String, String>,
    /// Comando do hook `Stop` para orquestradores Claude Code
    /// (normalmente `<este binário> hook stop`).
    pub stop_hook: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_fills_placeholders() {
        let ep = BrowserEndpoint {
            port: 9333,
            ws_url: "ws://127.0.0.1:9333/devtools/browser/x".to_owned(),
        };
        let spec = McpServerTemplate::playwright().render(&ep);
        assert_eq!(spec.args.last().unwrap(), "http://127.0.0.1:9333");
        assert_eq!(spec.name, "browser");
    }
}
