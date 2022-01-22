use {
    crate::{
        parse::Parse,
        query::{decl_at, decls_, Decl, DeclKind, Query},
        to_range, zeek, File,
    },
    itertools::Itertools,
    log::{error, warn},
    std::{
        collections::HashSet,
        fmt::Debug,
        path::PathBuf,
        sync::{Arc, Mutex},
    },
    tower_lsp::{
        jsonrpc::{Error, ErrorCode, Result},
        lsp_types::{
            CompletionItem, CompletionItemKind, CompletionOptions, CompletionParams,
            CompletionResponse, CreateFilesParams, DidChangeTextDocumentParams,
            DidOpenTextDocumentParams, DocumentSymbol, DocumentSymbolParams,
            DocumentSymbolResponse, Documentation, FileCreate, Hover, HoverContents, HoverParams,
            HoverProviderCapability, InitializeParams, InitializeResult, InitializedParams,
            LanguageString, Location, MarkedString, MessageType, OneOf, Position, Range,
            ServerCapabilities, SymbolInformation, SymbolKind, TextDocumentSyncCapability,
            TextDocumentSyncKind, Url, WorkspaceSymbolParams,
        },
        Client, LanguageServer, LspService, Server,
    },
    tracing::instrument,
};

#[salsa::database(
    crate::parse::ParseStorage,
    crate::query::QueryStorage,
    ServerStateStorage
)]
#[derive(Default)]
pub struct Database {
    storage: salsa::Storage<Self>,
}

#[salsa::query_group(ServerStateStorage)]
pub trait ServerState: Parse {
    #[salsa::input]
    fn prefixes(&self) -> Arc<Vec<PathBuf>>;

    #[salsa::input]
    fn files(&self) -> Arc<HashSet<Arc<Url>>>;
}

impl salsa::Database for Database {}

impl Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database").finish()
    }
}

#[derive(Debug, Default)]
struct State {
    db: Database,
}

#[derive(Debug)]
struct Backend {
    client: Client,
    state: Mutex<State>,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    #[instrument]
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        if let Ok(prefixes) = zeek::prefixes().await {
            if let Ok(mut state) = self.state.lock() {
                // Set up prefixes for normalization of system files.
                state.db.set_prefixes(Arc::new(prefixes));

                state.db.set_files(Arc::new(HashSet::new()));
            }
        }

        match zeek::system_files().await {
            Ok(files) => {
                self.did_create_files(CreateFilesParams {
                    files: files
                        .into_iter()
                        .filter_map(|f| {
                            Some(FileCreate {
                                uri: f.path.into_os_string().into_string().ok()?,
                            })
                        })
                        .collect(),
                })
                .await;
            }
            Err(e) => {
                self.client.log_message(MessageType::Error, e).await;
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::Full,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec!["$".into(), "?".into()]),
                    ..CompletionOptions::default()
                }),
                ..ServerCapabilities::default()
            },
            ..InitializeResult::default()
        })
    }

    #[instrument]
    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::Info, "server initialized!")
            .await;
    }

    #[instrument]
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    #[instrument]
    async fn did_create_files(&self, params: CreateFilesParams) {
        let _process = params
            .files
            .iter()
            .filter_map(|f| {
                let uri = if let Ok(uri) = Url::from_file_path(&f.uri) {
                    uri
                } else {
                    warn!(
                        "ignoring {} since its path cannot be converted to an URI",
                        &f.uri
                    );
                    return None;
                };

                let source = match std::fs::read_to_string(&f.uri) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("failed to read '{}': {}", &f.uri, e);
                        return None;
                    }
                };

                if let Ok(mut state) = self.state.lock() {
                    let file = Arc::new(File {
                        uri: uri.clone(),
                        source,
                    });

                    let uri = Arc::new(uri);

                    state.db.set_file(uri.clone(), file);

                    let mut files = state.db.files();
                    let files = Arc::make_mut(&mut files);
                    files.insert(uri);
                    state.db.set_files(Arc::new(files.clone()));
                };

                Some(())
            })
            .collect::<Vec<_>>();
    }

    #[instrument]
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let source = params.text_document.text;

        if let Ok(mut state) = self.state.lock() {
            let file = Arc::new(File {
                uri: uri.clone(),
                source,
            });

            let uri = Arc::new(uri);

            state.db.set_file(uri.clone(), file);

            let mut files = state.db.files();
            let files = Arc::make_mut(&mut files);
            files.insert(uri);
            state.db.set_files(Arc::new(files.clone()));
        }
    }

    #[instrument]
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let changes = params.content_changes;
        assert_eq!(
            changes.len(),
            1,
            "more than one change received even though we only advertize full update mode"
        );
        let changes = changes.get(0).unwrap();
        assert!(changes.range.is_none(), "unexpected diff mode");

        let uri = params.text_document.uri;

        let source = changes.text.to_string();
        let file = File {
            uri: uri.clone(),
            source,
        };

        if let Ok(mut state) = self.state.lock() {
            let uri = Arc::new(uri);
            state.db.set_file(uri.clone(), Arc::new(file));

            let mut files = state.db.files();
            let files = Arc::make_mut(&mut files);
            files.insert(uri);
            state.db.set_files(Arc::new(files.clone()));
        }
    }

    #[instrument]
    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let params = &params.text_document_position_params;

        let state = self
            .state
            .lock()
            .map_err(|_| Error::new(ErrorCode::InternalError))?;

        let file = state.db.file(Arc::new(params.text_document.uri.clone()));

        // TODO(bbannier): This is more of a demo and debugging tool for now. Eventually this
        // should return some nice rendering of the hovered node.

        let source = file.source.clone();

        let tree = state.db.parse(file);
        let tree = match tree.as_ref() {
            Some(t) => t,
            None => return Ok(None),
        };

        let node = match tree.named_descendant_for_position(&params.position) {
            Some(n) => n,
            None => return Ok(None),
        };

        let text = node.utf8_text(source.as_bytes()).map_err(|e| {
            error!("could not get source text: {}", e);
            Error::new(ErrorCode::InternalError)
        })?;

        let mut contents = vec![
            MarkedString::LanguageString(LanguageString {
                value: text.into(),
                language: "zeek".into(),
            }),
            #[cfg(debug_assertions)]
            MarkedString::LanguageString(LanguageString {
                value: node.to_sexp(),
                language: "lisp".into(),
            }),
        ];

        if node.kind() == "id" {
            let id = text;
            if let Some(decl) = decl_at(id, node, &source) {
                contents.push(MarkedString::String(decl.documentation));
            }
        }

        let hover = Hover {
            contents: HoverContents::Array(contents),
            range: to_range(node.range()).ok(),
        };

        Ok(Some(hover))
    }

    #[instrument]
    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let state = self
            .state
            .lock()
            .map_err(|_| Error::new(ErrorCode::InternalError))?;

        let file = state.db.file(Arc::new(params.text_document.uri));

        let symbol = |d: &Decl| -> DocumentSymbol {
            #[allow(deprecated)]
            DocumentSymbol {
                name: d.id.clone(),
                range: d.range,
                selection_range: d.selection_range,
                kind: to_symbol_kind(d.kind),
                deprecated: None,
                detail: None,
                tags: None,
                children: None,
            }
        };

        let modules = state
            .db
            .decls(file)
            .iter()
            .group_by(|d| &d.module)
            .into_iter()
            .map(|(m, decls)| {
                #[allow(deprecated)]
                DocumentSymbol {
                    name: format!("{}", m),
                    kind: SymbolKind::Module,
                    children: Some(decls.map(symbol).collect()),

                    // FIXME(bbannier): Weird ranges.
                    range: Range::new(Position::new(0, 0), Position::new(0, 0)),
                    selection_range: Range::new(Position::new(0, 0), Position::new(0, 0)),

                    deprecated: None,

                    detail: None,
                    tags: None,
                }
            })
            .collect();

        Ok(Some(DocumentSymbolResponse::Nested(modules)))
    }

    #[instrument]
    async fn symbol(&self, _: WorkspaceSymbolParams) -> Result<Option<Vec<SymbolInformation>>> {
        let state = self
            .state
            .lock()
            .map_err(|_| Error::new(ErrorCode::InternalError))?;

        let files = state.db.files();
        let symbols = files.iter().flat_map(|id| {
            state
                .db
                .decls(state.db.file(id.clone()))
                .iter()
                .map(|d| {
                    let url: &Url = &**id;

                    #[allow(deprecated)]
                    SymbolInformation {
                        name: format!("{}::{}", &d.module, &d.id),
                        kind: to_symbol_kind(d.kind),

                        location: Location::new(url.clone(), d.range),
                        container_name: Some(format!("{}", &d.module)),

                        tags: None,
                        deprecated: None,
                    }
                })
                .collect::<Vec<_>>()
        });

        Ok(Some(symbols.collect()))
    }

    #[instrument]
    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let position = params.text_document_position;

        let (tree, source) = {
            let state = self
                .state
                .lock()
                .map_err(|_| Error::new(ErrorCode::InternalError))?;

            let file = state.db.file(Arc::new(position.text_document.uri.clone()));

            let tree = match state.db.parse(file.clone()) {
                Some(t) => t,
                None => return Ok(None),
            };

            let source = file.source.clone();

            (tree, source)
        };

        let node = match tree.descendant_for_position(&position.position) {
            Some(n) => n,
            None => return Ok(None),
        };

        let items: Vec<_> = {
            let mut items = HashSet::new();
            let mut node = node;
            loop {
                for d in decls_(node, &source) {
                    items.insert(d);
                }

                node = match node.parent() {
                    Some(n) => n,
                    None => break,
                };
            }

            items.into_iter().map(to_completion_item).collect()
        };

        // TODO: Add an decls found in implicitly or explicitly loaded modules.

        Ok(Some(CompletionResponse::from(items)))
    }
}

fn to_symbol_kind(kind: DeclKind) -> SymbolKind {
    match kind {
        DeclKind::Global | DeclKind::Variable | DeclKind::Redef => SymbolKind::Variable,
        DeclKind::Option => SymbolKind::Property,
        DeclKind::Const => SymbolKind::Constant,
        DeclKind::RedefEnum => SymbolKind::Enum,
        DeclKind::RedefRecord => SymbolKind::Interface,
        DeclKind::Type => SymbolKind::Class,
        DeclKind::Func => SymbolKind::Function,
        DeclKind::Hook => SymbolKind::Operator,
        DeclKind::Event => SymbolKind::Event,
    }
}

fn to_completion_item(d: Decl) -> CompletionItem {
    CompletionItem {
        label: format!("{}::{}", d.module, d.id),
        kind: Some(to_completion_item_kind(d.kind)),
        documentation: Some(Documentation::String(d.documentation)),
        ..CompletionItem::default()
    }
}

fn to_completion_item_kind(kind: DeclKind) -> CompletionItemKind {
    match kind {
        DeclKind::Global | DeclKind::Variable | DeclKind::Redef => CompletionItemKind::Variable,
        DeclKind::Option => CompletionItemKind::Property,
        DeclKind::Const => CompletionItemKind::Constant,
        DeclKind::RedefEnum => CompletionItemKind::Enum,
        DeclKind::RedefRecord => CompletionItemKind::Interface,
        DeclKind::Type => CompletionItemKind::Class,
        DeclKind::Func => CompletionItemKind::Function,
        DeclKind::Hook => CompletionItemKind::Operator,
        DeclKind::Event => CompletionItemKind::Event,
    }
}

pub async fn run() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, messages) = LspService::new(|client| Backend {
        client,
        state: Mutex::default(),
    });
    Server::new(stdin, stdout)
        .interleave(messages)
        .serve(service)
        .await;
}
