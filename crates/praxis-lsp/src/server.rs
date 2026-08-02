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
    CompletionOptions, CompletionResponse, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentSymbolResponse,
    GotoDefinitionResponse, InitializeParams, InitializeResult, OneOf, PublishDiagnosticsParams,
    SemanticTokensFullOptions, SemanticTokensOptions, SemanticTokensServerCapabilities,
    ServerCapabilities, ServerInfo, SignatureHelpOptions, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions, TextDocumentSyncSaveOptions, Uri,
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
    let result = InitializeResult {
        capabilities: capabilities(encoding),
        server_info: Some(ServerInfo {
            name: SERVER_NAME.to_string(),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        }),
    };
    connection.initialize_finish(id, serde_json::to_value(result)?)?;

    let mut server = Server::new(encoding);
    server.main_loop(connection)
}

/// Exactly the M11 capabilities, and no more.
///
/// Advertising a capability the server does not implement is worse than not
/// advertising it: the editor stops offering its own fallback. Find references,
/// rename, workspace symbols, inlay hints and formatting are M12 and are absent
/// on purpose.
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
                // Full document only; deltas are M12.
                full: Some(SemanticTokensFullOptions::Bool(true)),
                range: Some(false),
                ..SemanticTokensOptions::default()
            },
        )),
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
            shutdown_requested: false,
        }
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
            lsp_types::request::Completion::METHOD => self
                .answer::<lsp_types::CompletionParams, _>(id, req, |s, uri, offset| {
                    let snapshot = s.snapshot(uri)?;
                    let ctx = snapshot.completion_context(offset);
                    Some(CompletionResponse::Array(crate::completion::items(
                        &snapshot, &ctx,
                    )))
                }),
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
