//! 多语言服务器管理器：同时管理多个 LSP 服务器实例。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tracing::{info, warn};
use url::Url;

use crate::client::{
    LspClient, LspCodeAction, LspDiagnostic, LspError, LspHover, LspLocation, LspRenameEdit,
    LspSymbol, apply_text_edits,
};
use crate::detect::{LspServerInfo, find_server_for_file, language_id_from_path};

/// 多语言 LSP 服务器管理器。
pub struct LspManager {
    root: PathBuf,
    clients: HashMap<String, LspClient>,
    server_infos: Vec<LspServerInfo>,
}

impl LspManager {
    /// 启动所有检测到的服务器。
    pub async fn start(root: &Path, servers: &[LspServerInfo]) -> Result<Self, LspError> {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let mut clients = HashMap::new();

        for server in servers {
            match LspClient::initialize(&root, server).await {
                Ok(client) => {
                    info!(server = %server.name, "LSP 服务器已启动");
                    for lang in &server.languages {
                        // 每个语言都指向同一服务器信息（按需时会重新启动）
                        // 启动成功后，只为第一个语言注册客户端
                        if clients.is_empty() || !clients.contains_key(lang) {
                            // 不能 clone LspClient，所以只为第一个语言存储
                        }
                        info!(lang = %lang, server = %server.name, "注册语言映射");
                    }
                    if let Some(first_lang) = server.languages.first() {
                        clients.insert(first_lang.clone(), client);
                    }
                }
                Err(e) => {
                    warn!(server = %server.name, error = %e, "启动 LSP 服务器失败");
                }
            }
        }

        Ok(Self {
            root,
            clients,
            server_infos: servers.to_vec(),
        })
    }

    /// 按需启动语言服务器。
    async fn ensure_client(&mut self, language_id: &str) -> Result<&mut LspClient, LspError> {
        if !self.clients.contains_key(language_id) {
            let server = self
                .server_infos
                .iter()
                .find(|s| s.languages.contains(&language_id.to_string()))
                .cloned();
            if let Some(server) = server {
                info!(server = %server.name, lang = %language_id, "按需启动 LSP 服务器");
                let client = LspClient::initialize(&self.root, &server).await?;
                self.clients.insert(language_id.to_string(), client);
            } else {
                return Err(LspError::Unsupported(format!(
                    "未找到语言 '{language_id}' 的 LSP 服务器"
                )));
            }
        }
        Ok(self.clients.get_mut(language_id).expect("刚检查/插入"))
    }

    /// 根据 URI 找到对应的客户端。
    async fn client_for_uri(&mut self, uri: &Url) -> Result<&mut LspClient, LspError> {
        let path = uri
            .to_file_path()
            .map_err(|_| LspError::InvalidUri(uri.to_string()))?;
        let lang_id = language_id_from_path(&path)
            .unwrap_or("plaintext")
            .to_string();

        if self.clients.contains_key(&lang_id) {
            return Ok(self.clients.get_mut(&lang_id).expect("刚检查"));
        }

        // 尝试按文件路径匹配服务器
        let primary_lang = find_server_for_file(&self.server_infos, &path)
            .and_then(|s| s.languages.first())
            .cloned()
            .unwrap_or(lang_id);

        self.ensure_client(&primary_lang).await
    }

    // ── 委托方法 ──────────────────────────────────────────────────────

    pub async fn open_document(&mut self, uri: &Url, text: &str) -> Result<(), LspError> {
        let path = uri
            .to_file_path()
            .map_err(|_| LspError::InvalidUri(uri.to_string()))?;
        let lang_id = language_id_from_path(&path).unwrap_or("plaintext");
        let client = self.ensure_client(lang_id).await?;
        client.open_document(uri, text, lang_id).await
    }

    pub async fn change_document(&mut self, uri: &Url, text: &str) -> Result<(), LspError> {
        self.client_for_uri(uri)
            .await?
            .change_document(uri, text)
            .await
    }

    pub async fn close_document(&mut self, uri: &Url) -> Result<(), LspError> {
        self.client_for_uri(uri).await?.close_document(uri).await
    }

    pub async fn diagnostics(&mut self, uri: &Url) -> Vec<LspDiagnostic> {
        match self.client_for_uri(uri).await {
            Ok(client) => client.diagnostics(uri).await,
            Err(e) => {
                warn!(error = %e, "获取诊断失败");
                vec![]
            }
        }
    }

    pub async fn all_diagnostics(&mut self) -> HashMap<String, Vec<LspDiagnostic>> {
        let mut all = HashMap::new();
        for client in self.clients.values() {
            all.extend(client.all_diagnostics().await);
        }
        all
    }

    pub async fn goto_definition(
        &mut self,
        uri: &Url,
        line: u32,
        character: u32,
    ) -> Result<Vec<LspLocation>, LspError> {
        self.client_for_uri(uri)
            .await?
            .goto_definition(uri, line, character)
            .await
    }

    pub async fn find_references(
        &mut self,
        uri: &Url,
        line: u32,
        character: u32,
    ) -> Result<Vec<LspLocation>, LspError> {
        self.client_for_uri(uri)
            .await?
            .find_references(uri, line, character)
            .await
    }

    pub async fn hover(
        &mut self,
        uri: &Url,
        line: u32,
        character: u32,
    ) -> Result<Option<LspHover>, LspError> {
        self.client_for_uri(uri)
            .await?
            .hover(uri, line, character)
            .await
    }

    pub async fn document_symbols(&mut self, uri: &Url) -> Result<Vec<LspSymbol>, LspError> {
        self.client_for_uri(uri).await?.document_symbols(uri).await
    }

    pub async fn workspace_symbols(&mut self, query: &str) -> Result<Vec<LspSymbol>, LspError> {
        let mut all = Vec::new();
        for client in self.clients.values_mut() {
            match client.workspace_symbols(query).await {
                Ok(s) => all.extend(s),
                Err(e) => warn!(error = %e, "工作区符号搜索失败"),
            }
        }
        Ok(all)
    }

    pub async fn rename(
        &mut self,
        uri: &Url,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Result<Vec<LspRenameEdit>, LspError> {
        self.client_for_uri(uri)
            .await?
            .rename(uri, line, character, new_name)
            .await
    }

    pub async fn code_actions(
        &mut self,
        uri: &Url,
        line: u32,
        character: u32,
    ) -> Result<Vec<LspCodeAction>, LspError> {
        self.client_for_uri(uri)
            .await?
            .code_actions(uri, line, character)
            .await
    }

    /// 格式化整个文档：读盘 → 同步文档 → `textDocument/formatting` → 应用 edits。
    ///
    /// 服务器不支持 formatting 或无 edits 时返回 `Ok(None)`；否则返回格式化后的完整文本。
    pub async fn format(&mut self, uri: &Url) -> Result<Option<String>, LspError> {
        let path = uri
            .to_file_path()
            .map_err(|_| LspError::InvalidUri(uri.to_string()))?;
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| LspError::Unsupported(format!("读取文件失败: {e}")))?;
        let lang_id = language_id_from_path(&path).unwrap_or("plaintext");
        let client = self.client_for_uri(uri).await?;
        client.ensure_document_open(uri, &text, lang_id).await?;
        let edits = client.formatting(uri).await?;
        if edits.is_empty() {
            return Ok(None);
        }
        Ok(Some(apply_text_edits(&text, &edits)))
    }

    /// 确保文档已 open 且内容同步（已 open 则 didChange，否则 didOpen）。
    pub async fn ensure_document_open(&mut self, uri: &Url, text: &str) -> Result<(), LspError> {
        let path = uri
            .to_file_path()
            .map_err(|_| LspError::InvalidUri(uri.to_string()))?;
        let lang_id = language_id_from_path(&path).unwrap_or("plaintext");
        self.client_for_uri(uri)
            .await?
            .ensure_document_open(uri, text, lang_id)
            .await
    }

    // ── 生命周期 ──────────────────────────────────────────────────────

    pub async fn shutdown_all(self) -> Result<(), LspError> {
        for (lang, client) in self.clients {
            info!(lang = %lang, "正在关闭 LSP 服务器");
            if let Err(e) = client.shutdown().await {
                warn!(lang = %lang, error = %e, "关闭 LSP 服务器失败");
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn active_languages(&self) -> Vec<&str> {
        self.clients.keys().map(String::as_str).collect()
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}
