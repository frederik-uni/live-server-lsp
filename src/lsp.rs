use rusty_live_server::{Dir, Error, File, FileSystemInterface, Signal};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::{read_dir, File as TokioFile, ReadDir};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, CodeActionResponse, Command,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, ExecuteCommandParams, InitializeParams, InitializeResult,
    InitializedParams, MessageType, Position, SaveOptions, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions,
};

use tower_lsp::{Client, LanguageServer, LspService, Server};

struct Backend {
    port: u16,
    public: bool,
    eager: bool,
    client: Client,
    threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
    workspace_folders: Arc<Mutex<HashMap<PathBuf, (String, LspFileService)>>>,
}

#[derive(Clone)]
struct LspFileService {
    eager: bool,
    port: Arc<Mutex<u16>>,
    root: Arc<PathBuf>,
    files: Arc<Mutex<HashMap<String, String>>>,
    sig: Signal,
}

struct LspDir {
    dir: ReadDir,
}

enum LspFile {
    Content(String),
    File(TokioFile),
}

impl LspFile {
    async fn new(
        files: Arc<Mutex<HashMap<String, String>>>,
        path: &Path,
        eager: bool,
    ) -> Result<Self, Error> {
        if !eager {
            return Ok(LspFile::File(TokioFile::open(path).await?));
        }
        let content = files
            .lock()
            .await
            .get(&format!("file://{}", path.to_str().unwrap_or_default()))
            .cloned();
        Ok(match content {
            Some(v) => LspFile::Content(v.to_string()),
            None => LspFile::File(TokioFile::open(path).await?),
        })
    }
}

impl File for LspFile {
    async fn read_to_end(&mut self) -> Vec<u8> {
        match self {
            LspFile::Content(c) => c.as_bytes().to_vec(),
            LspFile::File(file) => {
                let mut buffer = vec![];
                let _ = file.read_to_end(&mut buffer).await;
                buffer
            }
        }
    }
}

impl LspDir {
    async fn new(path: &Path) -> Result<Self, Error> {
        let dir = read_dir(path).await?;
        Ok(Self { dir })
    }
}

impl Dir for LspDir {
    async fn get_next(&mut self) -> Result<Option<PathBuf>, Error> {
        Ok(self.dir.next_entry().await?.map(|v| v.path()))
    }
}

impl FileSystemInterface for LspFileService {
    async fn get_dir(&self, path: &Path) -> Result<impl Dir, rusty_live_server::Error> {
        LspDir::new(path).await
    }

    async fn get_file(&self, path: &Path) -> Result<impl File, rusty_live_server::Error> {
        LspFile::new(self.files.clone(), path, self.eager).await
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let content = params.text_document.text;

        if let Some((_, service)) = self.get_workspace_for_file(&uri).await {
            let mut files = service.files.lock().await;
            if self.eager {
                files.insert(uri.clone(), content.clone());
            }
            self.update_file(&uri, &service, false).await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        if let Some((_, service)) = self.get_workspace_for_file(&uri).await {
            let message = format!("File saved: {}", uri);
            self.client.log_message(MessageType::INFO, message).await;
            self.update_file(&uri, &service, true).await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.to_string();

        if let Some((_, service)) = self.get_workspace_for_file(&uri).await {
            let mut files = service.files.lock().await;
            if let Some(file) = files.get_mut(&uri) {
                if self.eager {
                    for change in params.content_changes {
                        if let Some(range) = change.range {
                            let start = get_byte_index_from_position(file, range.start);
                            let end = get_byte_index_from_position(file, range.end);

                            file.replace_range(start..end, &change.text);
                        } else {
                            *file = change.text.clone();
                        }
                    }
                }
            }
            self.update_file(&uri, &service, false).await;
        }
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> tower_lsp::jsonrpc::Result<Option<Value>> {
        if params.command == "openProjectsWeb" {
            if let Some(project) = params.arguments.first().and_then(|arg| arg.as_str()) {
                if let Some((_, v)) = self.workspace_folders.lock().await.get(Path::new(project)) {
                    if let Err(e) =
                        webbrowser::open(&format!("http://127.0.0.1:{}", v.port.lock().await))
                    {
                        self.client
                            .show_message(
                                MessageType::WARNING,
                                format!("failed to open browser {}", e.to_string()),
                            )
                            .await;
                        return Err(tower_lsp::jsonrpc::Error::invalid_params(
                            "failed to open browser",
                        ));
                    }
                } else {
                    return Err(tower_lsp::jsonrpc::Error::invalid_params(
                        "URL argument invalid",
                    ));
                }
            } else {
                return Err(tower_lsp::jsonrpc::Error::invalid_params(
                    "URL argument missing",
                ));
            }
        } else if params.command == "openProjectsWeb" {
            self.client
                .show_message(MessageType::INFO, "run openProjectsWeb")
                .await;
            if let Err(e) = webbrowser::open(&format!("http://127.0.0.1:{}", self.port)) {
                self.client
                    .show_message(
                        MessageType::WARNING,
                        format!("failed to open browser {}", e.to_string()),
                    )
                    .await;
                return Err(tower_lsp::jsonrpc::Error::invalid_params(
                    "failed to open browser",
                ));
            }
        } else {
            return Err(tower_lsp::jsonrpc::Error::method_not_found());
        }
        Ok(None)
    }

    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> tower_lsp::jsonrpc::Result<InitializeResult> {
        if let Some(workspace_folders) = params.workspace_folders {
            let mut folders = self.workspace_folders.lock().await;
            for folder in workspace_folders {
                let name = if folder.name.is_empty() {
                    folder
                        .uri
                        .to_file_path()
                        .ok()
                        .and_then(|path| {
                            path.file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                        })
                        .unwrap_or_else(|| "Unnamed Workspace".to_string())
                } else {
                    folder.name.clone()
                };
                let path = folder
                    .uri
                    .to_file_path()
                    .unwrap_or_else(|_| PathBuf::from(&folder.uri.to_string()));
                let fs = LspFileService {
                    port: Arc::new(Mutex::new(self.port)),
                    sig: Signal::default(),
                    eager: self.eager,
                    files: Default::default(),
                    root: Arc::new(path.clone()),
                };
                folders.insert(path, (name, fs));
            }
        }
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                code_action_provider: Some(
                    tower_lsp::lsp_types::CodeActionProviderCapability::Simple(true),
                ),
                execute_command_provider: Some(tower_lsp::lsp_types::ExecuteCommandOptions {
                    commands: vec!["openProjectWeb".to_string(), "openProjectsWeb".to_string()],
                    ..Default::default()
                }),

                text_document_sync: Some(TextDocumentSyncCapability::Options(match self.eager {
                    true => TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        ..Default::default()
                    },
                    false => TextDocumentSyncOptions {
                        open_close: Some(false),
                        change: Some(TextDocumentSyncKind::NONE),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                        ..Default::default()
                    },
                })),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> tower_lsp::jsonrpc::Result<Option<CodeActionResponse>> {
        let mut actions = vec![];

        let uri = params.text_document.uri.to_string();

        if let Some((_, service)) = self.get_workspace_for_file(&uri).await {
            let port = *service.port.lock().await;
            let action = CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Open Project: 127.0.0.1:{}", port),
                kind: Some(CodeActionKind::EMPTY),
                command: Some(Command {
                    title: format!("Open Project: 127.0.0.1:{}", port),
                    command: "openProjectWeb".to_string(),
                    arguments: Some(vec![Value::from(
                        service.root.to_str().unwrap_or_default().to_string(),
                    )]),
                }),
                edit: None,
                diagnostics: None,
                is_preferred: Some(false),
                disabled: None,
                data: None,
            });
            actions.push(action);
        }

        Ok(Some(actions))
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "LiveServer Initialized!")
            .await;
        let folders = self.workspace_folders.lock().await;
        let mut threads = vec![];
        for (path, (name, fs)) in folders.iter() {
            let path = path.clone();
            let f = fs.clone();
            let public = self.public;
            threads.push(tokio::spawn(async move {
                loop {
                    let port = *f.port.clone().lock().await;
                    let _ = rusty_live_server::serve(
                        path.clone(),
                        port,
                        public,
                        Some(f.sig.clone()),
                        f.clone(),
                    )
                    .await;
                    *f.port.lock().await += 1;
                }
            }));
            self.client
                .log_message(
                    MessageType::INFO,
                    format!("Opend Workspace: {} at port {}", name, fs.port.lock().await),
                )
                .await;
        }
        *self.threads.lock().await = threads;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.to_string();

        if let Some((_, service)) = self.get_workspace_for_file(&uri).await {
            let mut files = service.files.lock().await;
            files.remove(&uri);
        }
    }

    async fn shutdown(&self) -> tower_lsp::jsonrpc::Result<()> {
        self.threads.lock().await.iter().for_each(|v| v.abort());
        Ok(())
    }
}

impl Backend {
    async fn get_workspace_for_file(&self, uri: &str) -> Option<(PathBuf, LspFileService)> {
        let folders = self.workspace_folders.lock().await;
        for (path, (_, service)) in folders.iter() {
            let file_path = Path::new(uri.strip_prefix("file://").unwrap_or(uri));
            if file_path.starts_with(&service.root.as_ref()) {
                return Some((path.clone(), service.clone()));
            }
        }
        None
    }

    async fn update_file(&self, uri: &str, service: &LspFileService, saved: bool) {
        self.client
            .log_message(MessageType::INFO, format!("File updated: {}", uri))
            .await;
        let abs = uri.strip_prefix("file://").unwrap_or(uri);
        let rel = abs
            .strip_prefix(service.root.to_str().unwrap_or_default())
            .unwrap_or(abs);
        self.call_custom_function(&service.root, Path::new(rel), saved)
            .await;
    }

    async fn call_custom_function(&self, workspace: &PathBuf, file_path: &Path, saved: bool) {
        if !self.eager && !saved {
            return;
        }
        self.client
            .log_message(MessageType::INFO, format!("reload"))
            .await;
        let mutex = self.workspace_folders.lock().await;
        if let Some((_, fs)) = mutex.get(workspace) {
            fs.sig.send_signal(file_path.to_path_buf());
        }
    }
}

pub async fn lsp(port: u16, public: bool, eager: bool) {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (client, server) = LspService::build(|client| Backend {
        client,
        workspace_folders: Default::default(),
        port,
        public,
        eager,
        threads: Default::default(),
    })
    .finish();

    Server::new(stdin, stdout, server).serve(client).await;
}

pub fn get_byte_index_from_position(s: &str, position: Position) -> usize {
    let line_start = index_of_first_char_in_line(s, position.line).unwrap_or(s.len());

    let char_index = line_start + position.character as usize;

    if char_index >= s.len() {
        s.char_indices().nth(s.len() - 1).unwrap().0
    } else {
        s.char_indices().nth(char_index).unwrap().0
    }
}

fn index_of_first_char_in_line(s: &str, line: u32) -> Option<usize> {
    let mut current_line = 0;
    let mut index = 0;

    for (i, c) in s.char_indices() {
        if c == '\n' {
            current_line += 1;
            if current_line == line {
                return Some(i + 1);
            }
        }
        index = i;
    }

    if current_line == line - 1 {
        return Some(index + 1);
    }

    None
}
