//! WS1's gate: a scripted JSON-RPC session over a pipe, driving the **real**
//! `praxis lsp` binary.
//!
//! This is the test that replaced `check.rs`'s "`praxis lsp` exits 2 and says it
//! is not implemented" assertion. Against `bcc5319`'s binary it fails at the
//! first read: the process printed a line to stderr and exited 2 without ever
//! framing a response.
//!
//! The framing is written out by hand rather than pulled from a client library:
//! what M11 promises is that *an editor* can talk to this process, and an editor
//! speaks `Content-Length` headers over stdio.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_praxis"))
}

/// A live `praxis lsp` process with framed stdio.
struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    fn start() -> Session {
        Session::start_with(&[])
    }

    /// A session with extra arguments after `lsp`.
    fn start_with(extra: &[&str]) -> Session {
        let mut child = Command::new(bin_path())
            .arg("lsp")
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn `praxis lsp`");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Session {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, message: &serde_json::Value) {
        let body = serde_json::to_string(message).expect("serializable");
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body)
            .expect("write to the server");
        self.stdin.flush().expect("flush");
    }

    /// Read one framed message. Panics if the stream ends first, which is what
    /// a server that exited instead of answering looks like.
    fn recv(&mut self) -> serde_json::Value {
        let mut length = None;
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read a header");
            assert!(read != 0, "the server closed the stream mid-handshake");
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
                length = Some(rest.trim().parse::<usize>().expect("a length"));
            }
        }
        let length = length.expect("every message is framed with a Content-Length");
        let mut body = vec![0u8; length];
        self.stdout.read_exact(&mut body).expect("read the body");
        serde_json::from_slice(&body).expect("a JSON body")
    }

    /// Read messages until one has this id, returning it. Notifications
    /// (diagnostics, in particular) arrive interleaved and are skipped.
    fn recv_response(&mut self, id: i64) -> serde_json::Value {
        for _ in 0..64 {
            let msg = self.recv();
            if msg.get("id").and_then(serde_json::Value::as_i64) == Some(id) {
                return msg;
            }
        }
        panic!("no response with id {id} arrived");
    }

    fn finish(mut self) -> i32 {
        drop(self.stdin);
        let status = self.child.wait().expect("the server exits");
        status.code().expect("not killed by a signal")
    }
}

fn initialize(session: &mut Session) -> serde_json::Value {
    session.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": null,
            "rootUri": null,
            "capabilities": {
                "general": { "positionEncodings": ["utf-8", "utf-16"] }
            }
        }
    }));
    let response = session.recv_response(1);
    session.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    }));
    response
}

/// **The handshake completes**, and the server advertises exactly the M11
/// capabilities.
#[test]
fn the_handshake_completes_and_advertises_the_m11_capabilities() {
    let mut session = Session::start();
    let response = initialize(&mut session);
    let caps = &response["result"]["capabilities"];

    assert_eq!(
        response["result"]["serverInfo"]["name"], "praxis-lsp",
        "the server names itself"
    );
    // UTF-8 was offered, so it must be what was chosen (ADR-096).
    assert_eq!(caps["positionEncoding"], "utf-8");

    assert_eq!(caps["hoverProvider"], true);
    assert_eq!(caps["definitionProvider"], true);
    assert_eq!(caps["documentSymbolProvider"], true);
    assert!(caps["completionProvider"].is_object());
    assert!(caps["signatureHelpProvider"].is_object());
    assert!(caps["semanticTokensProvider"].is_object());
    // Incremental document sync, not full.
    assert_eq!(caps["textDocumentSync"]["change"], 2);

    // …and **no more**. Advertising a capability M11 does not implement makes
    // the editor stop offering its own fallback (§19.12 owns these).
    for m12 in [
        "referencesProvider",
        "renameProvider",
        "workspaceSymbolProvider",
        "inlayHintProvider",
        "documentFormattingProvider",
        "codeActionProvider",
    ] {
        assert!(
            caps.get(m12).is_none_or(serde_json::Value::is_null),
            "`{m12}` is M12 and must not be advertised"
        );
    }

    shutdown(&mut session);
    assert_eq!(session.finish(), 0, "a clean shutdown exits 0");
}

/// An edit bumps the revision and the server answers about the **new** text.
///
/// The observable proof is hover: the file opens with `var x = 1`, the edit
/// makes it `"one"`, and hover over `x` moves from `Int` to `Text`. A server
/// that ignored `didChange` would answer `Int` twice.
#[test]
fn an_edit_changes_what_the_server_answers() {
    let mut session = Session::start();
    initialize(&mut session);

    let uri = "file:///edit.px";
    session.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": { "textDocument": {
            "uri": uri, "languageId": "praxis", "version": 1,
            "text": "var x = 1\nout(x)\n"
        }}
    }));

    session.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "textDocument/hover",
        "params": {
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": 4 }
        }
    }));
    let before = session.recv_response(2);
    let before_text = before["result"]["contents"]["value"]
        .as_str()
        .expect("hover markdown")
        .to_string();
    assert!(before_text.contains("Int"), "got {before_text}");

    // Replace the `1` with `"one"`.
    session.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didChange",
        "params": {
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{
                "range": {
                    "start": { "line": 0, "character": 8 },
                    "end":   { "line": 0, "character": 9 }
                },
                "text": "\"one\""
            }]
        }
    }));

    session.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 3, "method": "textDocument/hover",
        "params": {
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": 4 }
        }
    }));
    let after = session.recv_response(3);
    let after_text = after["result"]["contents"]["value"]
        .as_str()
        .expect("hover markdown")
        .to_string();
    assert!(
        after_text.contains("Text"),
        "the edit must reach the analysis, got {after_text}"
    );

    shutdown(&mut session);
    assert_eq!(session.finish(), 0);
}

/// Diagnostics are published for an open document, carrying the **registered**
/// code and the span — not merely "something was published".
#[test]
fn diagnostics_are_published_with_a_code_and_a_span() {
    let mut session = Session::start();
    initialize(&mut session);

    session.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": { "textDocument": {
            "uri": "file:///diag.px", "languageId": "praxis", "version": 1,
            "text": "var x: Int = \"t\"\n"
        }}
    }));

    let mut published = None;
    for _ in 0..16 {
        let msg = session.recv();
        if msg["method"] == "textDocument/publishDiagnostics" {
            published = Some(msg);
            break;
        }
    }
    let published = published.expect("an open document is diagnosed");
    let diags = published["params"]["diagnostics"]
        .as_array()
        .expect("an array");
    assert_eq!(diags.len(), 1, "one mismatch, got {diags:?}");
    assert_eq!(
        diags[0]["code"], "Y001",
        "the registered code, not a string"
    );
    assert_eq!(diags[0]["source"], "praxis");
    assert_eq!(diags[0]["range"]["start"]["line"], 0);
    assert_eq!(diags[0]["range"]["start"]["character"], 13);
    assert_eq!(diags[0]["range"]["end"]["character"], 16);

    shutdown(&mut session);
    assert_eq!(session.finish(), 0);
}

/// **`praxis lsp --stdio` completes the handshake.**
///
/// Several clients append `--stdio` to the server's argv to name a transport —
/// `vscode-languageclient` does it whenever `TransportKind.stdio` is set, which
/// is how the extension shipped and how it failed: clap rejected the flag and
/// exited 2 before a byte of protocol, and the client reported it as "the server
/// crashed 5 times".
///
/// The extension no longer sets `transport`, so it no longer passes the flag.
/// This test is the *other* half — the one that does not depend on the
/// extension being right, because the next client to pass it will not be ours.
/// `the_extensions_argv_names_only_subcommands_the_cli_has` could not catch it:
/// the flag was never in `argv.ts` to be read.
#[test]
fn the_server_accepts_the_stdio_flag_clients_append() {
    let mut session = Session::start_with(&["--stdio"]);
    let response = initialize(&mut session);
    assert_eq!(response["result"]["serverInfo"]["name"], "praxis-lsp");
    shutdown(&mut session);
    assert_eq!(session.finish(), 0);
}

/// A client that exits without shutting down gets `1`, which is the protocol's
/// own rule and not an arbitrary code.
#[test]
fn exiting_without_shutdown_is_a_nonzero_exit() {
    let mut session = Session::start();
    initialize(&mut session);
    session.send(&serde_json::json!({ "jsonrpc": "2.0", "method": "exit" }));
    assert_eq!(session.finish(), 1);
}

fn shutdown(session: &mut Session) {
    session.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 9999, "method": "shutdown", "params": null
    }));
    let _ = session.recv_response(9999);
    session.send(&serde_json::json!({ "jsonrpc": "2.0", "method": "exit" }));
}
