//! The transport and the message loop (WS1, ADR-095, §15.1).
//!
//! One synchronous loop on the main thread, over `lsp-server`'s framed stdio
//! transport. No async runtime: the workspace has no async dependency, every
//! M11 query is one file, and `rowan::SyntaxNode` is `!Send` so a worker thread
//! would pay a re-rooting cost to answer questions about a tree it cannot share.
//!
//! The loop owns the document store and the snapshot cache outright. There is no
//! lock, because there is no second thread that could want one.

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::Notification as _;
use lsp_types::request::Request as _;
use lsp_types::{
    CodeActionProviderCapability, CompletionOptions, CompletionResponse,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentSymbolResponse, GotoDefinitionResponse, InitializeParams,
    InitializeResult, OneOf, PublishDiagnosticsParams, RenameOptions, SemanticTokensFullOptions,
    SemanticTokensOptions, SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo,
    SignatureHelpOptions, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextDocumentSyncOptions, TextDocumentSyncSaveOptions, Uri, WorkDoneProgressOptions,
    WorkspaceSymbolResponse,
};

use crate::document::DocumentStore;
use crate::position::Encoding;
use crate::query::Analyzer;

/// How long the loop waits for the typing to stop before it publishes
/// diagnostics (§15.2: "diagnostics should update after a short debounce").
///
/// Short enough to feel immediate and long enough that a `.` mid-word does not
/// flash a report the next keystroke retracts — which is the concrete symptom
/// §8's residual risk names: an unfinished `.` swallows the next line's first
/// token and reports `Y110` on a line the user did not touch.
const DEBOUNCE: Duration = Duration::from_millis(150);

/// The server's name and version, reported at `initialize`.
const SERVER_NAME: &str = "praxis-lsp";

/// Run the language server over stdio until the client says `exit`.
///
/// Returns the process exit code: `0` after a clean `shutdown`/`exit`, `1` when
/// the client exited without shutting down (the protocol's own rule).
///
/// # Errors
/// Returns the transport error when stdio cannot be framed or the connection
/// breaks in a way that is not an orderly disconnect.
pub fn run() -> anyhow::Result<i32> {
    let (connection, io_threads) = Connection::stdio();
    let code = serve(&connection)?;
    drop(connection);
    io_threads.join()?;
    Ok(code)
}

/// The loop itself, over any [`Connection`] — stdio in production, an in-memory
/// pair in tests.
pub fn serve(connection: &Connection) -> anyhow::Result<i32> {
    let (id, params) = connection.initialize_start()?;
    let params: InitializeParams = serde_json::from_value(params)?;
    let encoding = Encoding::negotiate(
        params
            .capabilities
            .general
            .as_ref()
            .and_then(|g| g.position_encodings.as_deref()),
    );
    let roots = workspace_roots(&params);
    let result = InitializeResult {
        capabilities: capabilities(encoding),
        server_info: Some(ServerInfo {
            name: SERVER_NAME.to_string(),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        }),
    };
    connection.initialize_finish(id, serde_json::to_value(result)?)?;

    let mut server = Server::new(encoding);
    server.set_roots(roots);
    server.main_loop(connection)
}

/// The folders `workspace/symbol` searches.
///
/// `workspaceFolders` when the client sent them, and the deprecated `rootUri`
/// when it did not — a client may still send only the latter, and a symbol
/// picker that answered nothing there would look broken rather than
/// unconfigured. A client with neither is in single-file mode, and
/// [`crate::workspace::open_document_symbols`] is what answers for it.
fn workspace_roots(params: &InitializeParams) -> Vec<std::path::PathBuf> {
    #[allow(deprecated)]
    let from_root = params.root_uri.iter();
    params
        .workspace_folders
        .iter()
        .flatten()
        .map(|f| &f.uri)
        .chain(from_root)
        .filter_map(crate::workspace::uri_to_path)
        .collect()
}

/// Exactly what this server implements, and no more.
///
/// Advertising a capability the server does not implement is worse than not
/// advertising it: the editor stops offering its own fallback. M12 adds find
/// references, rename (with `prepareProvider`, so a position that cannot be
/// renamed says so before the user types a new name), workspace symbols, inlay
/// hints and code actions.
///
/// **`documentFormattingProvider` stays absent.** §19.12 lists a stable
/// formatter and it is deliberately not part of this milestone — see the M12
/// handover. An editor that is told this server formats would stop offering its
/// own behaviour and then do nothing on `Format Document`, which is a worse
/// state than the one where the feature is simply missing.
fn capabilities(encoding: Encoding) -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(encoding.kind()),
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                ..TextDocumentSyncOptions::default()
            },
        )),
        hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions {
            // `.` for members; the three template characters because completion
            // inside a parser expression fires on text that is not yet an
            // expression.
            //
            // **Registering a character is not agreeing to answer on it.** `{`
            // and `:` mean something in a template and something else entirely
            // in ordinary code, and the editor cannot tell which it just typed
            // — only the resolved context can. So the list stays wide and
            // `completion::trigger_answers_here` narrows it per request.
            trigger_characters: Some(vec![
                ".".to_string(),
                "`".to_string(),
                "{".to_string(),
                ":".to_string(),
            ]),
            ..CompletionOptions::default()
        }),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
            retrigger_characters: Some(vec![",".to_string()]),
            ..SignatureHelpOptions::default()
        }),
        definition_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: crate::semantic::legend(),
                // Full document only; deltas remain out of scope.
                full: Some(SemanticTokensFullOptions::Bool(true)),
                range: Some(false),
                ..SemanticTokensOptions::default()
            },
        )),
        references_provider: Some(OneOf::Left(true)),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        inlay_hint_provider: Some(OneOf::Left(true)),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        ..ServerCapabilities::default()
    }
}

/// The server's whole mutable state. One owner, one thread (ADR-095).
pub struct Server {
    docs: DocumentStore,
    analyzer: Analyzer,
    encoding: Encoding,
    /// URIs whose diagnostics are owed once the typing stops.
    dirty: Vec<Uri>,
    /// The workspace folders `workspace/symbol` searches. Empty in single-file
    /// mode, where the open documents are the workspace.
    roots: Vec<std::path::PathBuf>,
    shutdown_requested: bool,
}

impl Server {
    #[must_use]
    pub fn new(encoding: Encoding) -> Server {
        Server {
            docs: DocumentStore::new(),
            analyzer: Analyzer::new(),
            encoding,
            dirty: Vec::new(),
            roots: Vec::new(),
            shutdown_requested: false,
        }
    }

    /// Point the workspace queries at these folders. Called once, from the
    /// `initialize` handshake.
    pub fn set_roots(&mut self, roots: Vec<std::path::PathBuf>) {
        self.roots = roots;
    }

    fn main_loop(&mut self, connection: &Connection) -> anyhow::Result<i32> {
        loop {
            // Block for the first message, then take everything already queued
            // behind it. Batching is what makes `$/cancelRequest` mean something
            // in a synchronous loop: a cancel that arrived while an earlier
            // request was being served is in this batch, ahead of the request it
            // cancels being *processed*.
            let first = if self.dirty.is_empty() {
                match connection.receiver.recv() {
                    Ok(msg) => Some(msg),
                    Err(_) => return Ok(self.exit_code()),
                }
            } else {
                match connection.receiver.recv_timeout(DEBOUNCE) {
                    Ok(msg) => Some(msg),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        return Ok(self.exit_code())
                    }
                }
            };

            let Some(first) = first else {
                // The typing stopped: this is the debounce firing.
                self.publish_dirty(connection);
                continue;
            };

            let mut batch = VecDeque::new();
            batch.push_back(first);
            while let Ok(msg) = connection.receiver.try_recv() {
                batch.push_back(msg);
            }

            let cancelled = cancelled_ids(&batch);
            for msg in batch {
                match msg {
                    Message::Request(req) => {
                        if req.method == lsp_types::request::Shutdown::METHOD {
                            self.shutdown_requested = true;
                            send(
                                connection,
                                Response::new_ok(req.id, serde_json::Value::Null),
                            );
                            continue;
                        }
                        if self.shutdown_requested {
                            // The protocol's rule: a request that arrives after
                            // `shutdown` errors rather than being served. The
                            // client has said it is leaving, and answering it
                            // about a document store it is about to discard is
                            // work nobody reads.
                            send(
                                connection,
                                Response::new_err(
                                    req.id,
                                    ErrorCode::InvalidRequest as i32,
                                    "the server has been asked to shut down".to_string(),
                                ),
                            );
                            continue;
                        }
                        if cancelled.contains(&req.id) {
                            send(
                                connection,
                                Response::new_err(
                                    req.id,
                                    ErrorCode::RequestCanceled as i32,
                                    "request cancelled".to_string(),
                                ),
                            );
                            continue;
                        }
                        let response = self.handle_request(req);
                        send(connection, response);
                    }
                    Message::Notification(note) => {
                        if note.method == lsp_types::notification::Exit::METHOD {
                            return Ok(self.exit_code());
                        }
                        self.handle_notification(&note);
                    }
                    Message::Response(_) => {}
                }
            }
        }
    }

    /// The protocol's rule: `0` if the client asked to shut down first, `1` if
    /// it just left.
    fn exit_code(&self) -> i32 {
        i32::from(!self.shutdown_requested)
    }

    fn handle_request(&mut self, req: Request) -> Response {
        let id = req.id.clone();
        match req.method.as_str() {
            lsp_types::request::HoverRequest::METHOD => {
                self.answer::<lsp_types::HoverParams, _>(id, req, |s, uri, offset| {
                    let snapshot = s.snapshot(uri)?;
                    crate::hover::hover(&snapshot, offset, s.encoding)
                })
            }
            lsp_types::request::Completion::METHOD => {
                let params: lsp_types::CompletionParams = match parse_params(&req) {
                    Ok(p) => p,
                    Err(e) => return invalid(id, &e),
                };
                let uri = params.text_document_position.text_document.uri.clone();
                let Some(doc) = self.docs.get(&uri) else {
                    return ok(id, serde_json::Value::Null);
                };
                let offset = doc
                    .positions()
                    .offset(params.text_document_position.position, self.encoding);
                let Some(snapshot) = self.snapshot(&uri) else {
                    return ok(id, serde_json::Value::Null);
                };
                let ctx = snapshot.completion_context(offset);
                // A menu the editor opened on a typed character is only owed
                // where that character means what it was registered for
                // (`completion::trigger_answers_here`). An empty list closes
                // the widget; the next word character starts a fresh request
                // that is not gated at all.
                let items = match typed_trigger(params.context.as_ref()) {
                    Some(c) if !crate::completion::trigger_answers_here(c, &ctx) => Vec::new(),
                    _ => crate::completion::items(&snapshot, &ctx),
                };
                ok(id, CompletionResponse::Array(items))
            }
            lsp_types::request::SignatureHelpRequest::METHOD => self
                .answer::<lsp_types::SignatureHelpParams, _>(id, req, |s, uri, offset| {
                    let snapshot = s.snapshot(uri)?;
                    crate::signature::signature_help(&snapshot, offset)
                }),
            lsp_types::request::GotoDefinition::METHOD => self
                .answer::<lsp_types::GotoDefinitionParams, _>(id, req, |s, uri, offset| {
                    let snapshot = s.snapshot(uri)?;
                    crate::navigation::goto_definition(&snapshot, offset, uri, s.encoding)
                        .map(GotoDefinitionResponse::Scalar)
                }),
            lsp_types::request::DocumentSymbolRequest::METHOD => {
                let params: lsp_types::DocumentSymbolParams = match parse_params(&req) {
                    Ok(p) => p,
                    Err(e) => return invalid(id, &e),
                };
                let uri = params.text_document.uri;
                let symbols = self
                    .snapshot(&uri)
                    .map(|s| crate::navigation::document_symbols(&s, self.encoding))
                    .unwrap_or_default();
                ok(id, DocumentSymbolResponse::Nested(symbols))
            }
            lsp_types::request::References::METHOD => {
                let params: lsp_types::ReferenceParams = match parse_params(&req) {
                    Ok(p) => p,
                    Err(e) => return invalid(id, &e),
                };
                let include = params.context.include_declaration;
                let uri = params.text_document_position.text_document.uri.clone();
                let Some(doc) = self.docs.get(&uri) else {
                    return ok(id, serde_json::Value::Null);
                };
                let offset = doc
                    .positions()
                    .offset(params.text_document_position.position, self.encoding);
                let Some(snapshot) = self.snapshot(&uri) else {
                    return ok(id, serde_json::Value::Null);
                };
                match crate::navigation::references(&snapshot, offset, &uri, self.encoding, include)
                {
                    Some(locations) => ok(id, locations),
                    None => ok(id, serde_json::Value::Null),
                }
            }
            lsp_types::request::PrepareRenameRequest::METHOD => {
                self.answer::<lsp_types::TextDocumentPositionParams, _>(
                    id,
                    req,
                    |s, uri, offset| {
                        let snapshot = s.snapshot(uri)?;
                        crate::rename::prepare(&snapshot, offset, s.encoding)
                    },
                )
            }
            lsp_types::request::Rename::METHOD => {
                let params: lsp_types::RenameParams = match parse_params(&req) {
                    Ok(p) => p,
                    Err(e) => return invalid(id, &e),
                };
                let uri = params.text_document_position.text_document.uri.clone();
                let Some(doc) = self.docs.get(&uri) else {
                    return ok(id, serde_json::Value::Null);
                };
                let offset = doc
                    .positions()
                    .offset(params.text_document_position.position, self.encoding);
                let Some(snapshot) = self.snapshot(&uri) else {
                    return ok(id, serde_json::Value::Null);
                };
                match crate::rename::rename(
                    &snapshot,
                    offset,
                    &params.new_name,
                    &uri,
                    self.encoding,
                ) {
                    Ok(edit) => ok(id, edit),
                    // **An error, not an empty edit.** A refusal is a sentence
                    // the user needs to read — which name would have been safe —
                    // and a client shows a request error and silently ignores an
                    // edit that changes nothing.
                    Err(refusal) => {
                        Response::new_err(id, ErrorCode::RequestFailed as i32, refusal.to_string())
                    }
                }
            }
            lsp_types::request::WorkspaceSymbolRequest::METHOD => {
                let params: lsp_types::WorkspaceSymbolParams = match parse_params(&req) {
                    Ok(p) => p,
                    Err(e) => return invalid(id, &e),
                };
                let symbols = if self.roots.is_empty() {
                    crate::workspace::open_document_symbols(
                        &params.query,
                        &self.docs,
                        self.encoding,
                    )
                } else {
                    crate::workspace::symbols(&params.query, &self.roots, &self.docs, self.encoding)
                };
                ok(id, WorkspaceSymbolResponse::Nested(symbols))
            }
            lsp_types::request::InlayHintRequest::METHOD => {
                let params: lsp_types::InlayHintParams = match parse_params(&req) {
                    Ok(p) => p,
                    Err(e) => return invalid(id, &e),
                };
                let uri = params.text_document.uri;
                match self.snapshot(&uri) {
                    Some(s) => ok(id, crate::inlay::hints(&s, params.range, self.encoding)),
                    None => ok(id, serde_json::Value::Null),
                }
            }
            lsp_types::request::CodeActionRequest::METHOD => {
                let params: lsp_types::CodeActionParams = match parse_params(&req) {
                    Ok(p) => p,
                    Err(e) => return invalid(id, &e),
                };
                let uri = params.text_document.uri;
                match self.snapshot(&uri) {
                    Some(s) => ok(
                        id,
                        crate::code_action::actions(&s, params.range, &uri, self.encoding),
                    ),
                    None => ok(id, serde_json::Value::Null),
                }
            }
            lsp_types::request::SemanticTokensFullRequest::METHOD => {
                let params: lsp_types::SemanticTokensParams = match parse_params(&req) {
                    Ok(p) => p,
                    Err(e) => return invalid(id, &e),
                };
                let uri = params.text_document.uri;
                match self.snapshot(&uri) {
                    Some(s) => ok(id, crate::semantic::tokens(&s, self.encoding)),
                    None => ok(id, serde_json::Value::Null),
                }
            }
            other => Response::new_err(
                id,
                ErrorCode::MethodNotFound as i32,
                format!("`{other}` is not implemented by this server"),
            ),
        }
    }

    /// Answer a position-parameterized request.
    fn answer<T, R>(
        &mut self,
        id: RequestId,
        req: Request,
        f: impl FnOnce(&mut Self, &Uri, u32) -> Option<R>,
    ) -> Response
    where
        T: serde::de::DeserializeOwned + HasPosition,
        R: serde::Serialize,
    {
        let params: T = match parse_params(&req) {
            Ok(p) => p,
            Err(e) => return invalid(id, &e),
        };
        let (uri, position) = params.position();
        let Some(doc) = self.docs.get(&uri) else {
            return ok(id, serde_json::Value::Null);
        };
        let offset = doc.positions().offset(position, self.encoding);
        match f(self, &uri, offset) {
            Some(value) => ok(id, value),
            None => ok(id, serde_json::Value::Null),
        }
    }

    fn handle_notification(&mut self, note: &Notification) {
        match note.method.as_str() {
            lsp_types::notification::DidOpenTextDocument::METHOD => {
                let Ok(p) =
                    serde_json::from_value::<DidOpenTextDocumentParams>(note.params.clone())
                else {
                    return;
                };
                let doc = p.text_document;
                self.docs.open(doc.uri.clone(), doc.text, doc.version);
                self.mark_dirty(doc.uri);
            }
            lsp_types::notification::DidChangeTextDocument::METHOD => {
                let Ok(p) =
                    serde_json::from_value::<DidChangeTextDocumentParams>(note.params.clone())
                else {
                    return;
                };
                let uri = p.text_document.uri;
                let encoding = self.encoding;
                if let Some(doc) = self.docs.get_mut(&uri) {
                    for change in &p.content_changes {
                        doc.apply(change, encoding);
                    }
                    doc.set_version(p.text_document.version);
                }
                self.mark_dirty(uri);
            }
            lsp_types::notification::DidSaveTextDocument::METHOD => {
                let Ok(p) =
                    serde_json::from_value::<DidSaveTextDocumentParams>(note.params.clone())
                else {
                    return;
                };
                self.mark_dirty(p.text_document.uri);
            }
            lsp_types::notification::DidCloseTextDocument::METHOD => {
                let Ok(p) =
                    serde_json::from_value::<DidCloseTextDocumentParams>(note.params.clone())
                else {
                    return;
                };
                let uri = p.text_document.uri;
                self.docs.close(&uri);
                self.analyzer.forget(uri.as_str());
                self.dirty.retain(|u| u != &uri);
            }
            _ => {}
        }
    }

    fn mark_dirty(&mut self, uri: Uri) {
        if !self.dirty.contains(&uri) {
            self.dirty.push(uri);
        }
    }

    /// Publish every owed report. Called when the debounce fires.
    fn publish_dirty(&mut self, connection: &Connection) {
        for uri in std::mem::take(&mut self.dirty) {
            let Some(params) = self.diagnostics_for(&uri) else {
                continue;
            };
            let Ok(value) = serde_json::to_value(params) else {
                continue;
            };
            send(
                connection,
                Notification {
                    method: lsp_types::notification::PublishDiagnostics::METHOD.to_string(),
                    params: value,
                },
            );
        }
    }

    /// The report for one URI, or `None` if it is not open.
    ///
    /// Public so the WS3 test can assert the code and the span without running
    /// a transport.
    pub fn diagnostics_for(&mut self, uri: &Uri) -> Option<PublishDiagnosticsParams> {
        let version = self.docs.get(uri)?.version();
        let snapshot = self.snapshot(uri)?;
        let diags = snapshot.diagnostics();
        Some(PublishDiagnosticsParams {
            uri: uri.clone(),
            diagnostics: crate::diagnostics::all_to_lsp(
                &diags,
                uri,
                snapshot.positions(),
                self.encoding,
            ),
            version: Some(version),
        })
    }

    /// The snapshot for an open URI, built or reused.
    pub fn snapshot(&mut self, uri: &Uri) -> Option<std::rc::Rc<crate::query::Snapshot>> {
        let doc = self.docs.get(uri)?;
        Some(self.analyzer.snapshot(uri.as_str(), doc))
    }

    /// Open a document without a transport. For tests, and for `praxis check`'s
    /// sibling paths that want the server's own view of a file.
    pub fn open(&mut self, uri: Uri, text: String, version: i32) {
        self.docs.open(uri, text, version);
    }

    #[must_use]
    pub fn documents(&self) -> &DocumentStore {
        &self.docs
    }

    #[must_use]
    pub fn documents_mut(&mut self) -> &mut DocumentStore {
        &mut self.docs
    }

    #[must_use]
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }
}

/// The ids named by every `$/cancelRequest` in a batch.
fn cancelled_ids(batch: &VecDeque<Message>) -> HashSet<RequestId> {
    batch
        .iter()
        .filter_map(|m| match m {
            Message::Notification(n) if n.method == lsp_types::notification::Cancel::METHOD => {
                serde_json::from_value::<lsp_types::CancelParams>(n.params.clone()).ok()
            }
            _ => None,
        })
        .map(|p| match p.id {
            lsp_types::NumberOrString::Number(n) => RequestId::from(n),
            lsp_types::NumberOrString::String(s) => RequestId::from(s),
        })
        .collect()
}

/// A request whose parameters name a document position.
trait HasPosition {
    fn position(self) -> (Uri, lsp_types::Position);
}

impl HasPosition for lsp_types::HoverParams {
    fn position(self) -> (Uri, lsp_types::Position) {
        (
            self.text_document_position_params.text_document.uri,
            self.text_document_position_params.position,
        )
    }
}

impl HasPosition for lsp_types::CompletionParams {
    fn position(self) -> (Uri, lsp_types::Position) {
        (
            self.text_document_position.text_document.uri,
            self.text_document_position.position,
        )
    }
}

impl HasPosition for lsp_types::SignatureHelpParams {
    fn position(self) -> (Uri, lsp_types::Position) {
        (
            self.text_document_position_params.text_document.uri,
            self.text_document_position_params.position,
        )
    }
}

impl HasPosition for lsp_types::GotoDefinitionParams {
    fn position(self) -> (Uri, lsp_types::Position) {
        (
            self.text_document_position_params.text_document.uri,
            self.text_document_position_params.position,
        )
    }
}

impl HasPosition for lsp_types::TextDocumentPositionParams {
    fn position(self) -> (Uri, lsp_types::Position) {
        (self.text_document.uri, self.position)
    }
}

/// The character the editor fired a completion request on, when a character is
/// what fired it.
///
/// `INVOKED` covers both <kbd>Ctrl</kbd>+<kbd>Space</kbd> and the editor's own
/// suggest-as-you-type, and carries no character; a client that omits `context`
/// altogether says no more than that. Both are requests, not reflexes, and
/// [`crate::completion::trigger_answers_here`] never sees them.
fn typed_trigger(ctx: Option<&lsp_types::CompletionContext>) -> Option<&str> {
    let ctx = ctx?;
    if ctx.trigger_kind != lsp_types::CompletionTriggerKind::TRIGGER_CHARACTER {
        return None;
    }
    ctx.trigger_character.as_deref()
}

fn parse_params<T: serde::de::DeserializeOwned>(req: &Request) -> Result<T, serde_json::Error> {
    serde_json::from_value(req.params.clone())
}

fn ok<T: serde::Serialize>(id: RequestId, value: T) -> Response {
    match serde_json::to_value(value) {
        Ok(v) => Response::new_ok(id, v),
        Err(e) => Response::new_err(id, ErrorCode::InternalError as i32, e.to_string()),
    }
}

fn invalid(id: RequestId, err: &serde_json::Error) -> Response {
    Response::new_err(id, ErrorCode::InvalidParams as i32, err.to_string())
}

fn send(connection: &Connection, msg: impl Into<Message>) {
    // A send failure means the client is gone; the loop's next receive reports
    // it as a disconnect. Nothing here can do better than let that happen.
    let _ = connection.sender.send(msg.into());
}
