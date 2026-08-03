use std::path::{Path, PathBuf};

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use ly_lsp::document_graph::DocumentGraph;
use ly_lsp::semantic_tokens;

#[derive(Debug)]
struct Backend {
    client: Client,
    documents: DocumentGraph,
}

impl Backend {
    fn new(client: Client) -> Self {
        Self {
            client,
            documents: DocumentGraph::new(),
        }
    }

    /// Computes and publishes syntax diagnostics for `uri`.
    async fn publish_diagnostics(&self, uri: Url, version: Option<i32>) {
        let diagnostics = self.documents.diagnostics(&uri);
        self.client
            .publish_diagnostics(uri, diagnostics, version)
            .await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // The client tells us where LilyPond's `lilypond-words` file lives so we
        // can recognise built-in commands; without it, undefined-reference
        // diagnostics stay off.
        let options = params.initialization_options;

        if let Some(path) = options
            .as_ref()
            .and_then(|opts| opts.get("lilypondWordsPath"))
            .and_then(|value| value.as_str())
            && !self.documents.load_vocabulary(Path::new(path))
        {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!("could not load LilyPond words from {path}"),
                )
                .await;
        }

        // The `-I` include search directories the user has configured.
        if let Some(paths) = options
            .as_ref()
            .and_then(|opts| opts.get("includePaths"))
            .and_then(|value| value.as_array())
        {
            let paths = paths
                .iter()
                .filter_map(|value| value.as_str())
                .map(PathBuf::from)
                .collect();
            self.documents.set_search_paths(paths);
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        // Edits are computed lazily, in `code_action_resolve`.
                        resolve_provider: Some(true),
                        ..CodeActionOptions::default()
                    },
                )),
                rename_provider: Some(OneOf::Left(true)),
                // Signature help retriggers on the space after a command word
                // (as each further argument is typed) and on a fresh `\`
                // (typing the start of a nested command inside a body); `(`
                // means nothing in LilyPond's own syntax, so it's not listed.
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec![" ".to_string(), "\\".to_string()]),
                    retrigger_characters: Some(vec![" ".to_string()]),
                    ..SignatureHelpOptions::default()
                }),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![" ".to_string(), "\\".to_string()]),
                    ..CompletionOptions::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: semantic_tokens::legend(),
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            ..SemanticTokensOptions::default()
                        },
                    ),
                ),
                ..ServerCapabilities::default()
            },
            server_info: Some(ServerInfo {
                name: "ly-lsp".to_string(),
                version: Some("0.1.0".to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "LilyPond LSP initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        self.documents.open(doc.uri.clone(), doc.text);
        self.publish_diagnostics(doc.uri, Some(doc.version)).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        // Incremental sync: apply each change in order, reparsing against the
        // previous tree.
        let version = params.text_document.version;
        let uri = params.text_document.uri;
        self.documents.change(&uri, params.content_changes);
        self.publish_diagnostics(uri, Some(version)).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.close(&uri);
        // Clear any squiggles for the now-closed file.
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pos = params.text_document_position_params;
        let locations = self
            .documents
            .goto_definition(&pos.text_document.uri, pos.position);
        Ok((!locations.is_empty()).then_some(GotoDefinitionResponse::Array(locations)))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let pos = params.text_document_position_params;
        let highlights = self
            .documents
            .document_highlights(&pos.text_document.uri, pos.position);
        Ok(Some(highlights))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let pos = params.text_document_position;
        let locations = self.documents.references(
            &pos.text_document.uri,
            pos.position,
            params.context.include_declaration,
        );
        Ok(Some(locations))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let actions = self
            .documents
            .code_actions(&params.text_document.uri, params.range);
        Ok((!actions.is_empty()).then_some(actions))
    }

    async fn code_action_resolve(&self, action: CodeAction) -> Result<CodeAction> {
        Ok(self.documents.resolve_code_action(action))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let pos = params.text_document_position;
        Ok(self
            .documents
            .rename(&pos.text_document.uri, pos.position, &params.new_name))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let pos = params.text_document_position_params;
        Ok(self
            .documents
            .signature_help(&pos.text_document.uri, pos.position))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let pos = params.text_document_position;
        let items = self
            .documents
            .completions(&pos.text_document.uri, pos.position);
        Ok((!items.is_empty()).then_some(CompletionResponse::Array(items)))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let pos = params.text_document_position_params;
        Ok(self.documents.hover(&pos.text_document.uri, pos.position))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let data = self
            .documents
            .semantic_tokens_full(&params.text_document.uri);
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Backend::new);

    Server::new(stdin, stdout, socket).serve(service).await;
}
