//! A scripted JSON-RPC session over a pipe, driving the **real** `praxis lsp`
//! binary.
//!
//! The framing is written out by hand rather than pulled from a client library:
//! what is being promised is that *an editor* can talk to this process, and an
//! editor speaks `Content-Length` headers over stdio.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

mod common;

use common::bin_path;

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

/// **The handshake completes**, and the server advertises exactly what it
/// implements.
#[test]
fn the_handshake_completes_and_advertises_the_implemented_capabilities() {
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

    // The navigation and edit providers.
    assert_eq!(caps["referencesProvider"], true);
    assert_eq!(caps["workspaceSymbolProvider"], true);
    assert_eq!(caps["inlayHintProvider"], true);
    assert_eq!(caps["codeActionProvider"], true);
    // Rename advertises `prepareProvider`, so a client asks whether a position
    // can be renamed *before* the user types a replacement.
    assert_eq!(caps["renameProvider"]["prepareProvider"], true);

    // …and **no more**. Advertising a capability the server does not implement
    // makes the editor stop offering its own fallback — Praxis has no
    // formatter, so a client must keep whatever it would do by itself.
    for absent in [
        "documentFormattingProvider",
        "documentRangeFormattingProvider",
        "documentOnTypeFormattingProvider",
    ] {
        assert!(
            caps.get(absent).is_none_or(serde_json::Value::is_null),
            "`{absent}` is not implemented and must not be advertised"
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

/// The navigation and edit requests over the wire, against the real binary.
///
/// The query layer has its own gates (`praxis-lsp/tests/editor_actions.rs`); what this adds
/// is that each method is **routed and serialized** — a handler the loop does not
/// dispatch answers `MethodNotFound`, and a response shape a client cannot read
/// is invisible to a test that calls the function directly.
#[test]
fn the_editor_action_requests_answer_over_the_wire() {
    let mut session = Session::start();
    initialize(&mut session);

    let uri = "file:///editor-actions.px";
    session.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": { "textDocument": {
            "uri": uri, "languageId": "praxis", "version": 1,
            "text": "fn foo(a) { a + 1 }\nvar total = foo(1)\nout(total)\n"
        }}
    }));

    // References: the `total` declaration and the `out(total)` that reads it.
    session.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 10, "method": "textDocument/references",
        "params": {
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": 4 },
            "context": { "includeDeclaration": true }
        }
    }));
    let refs = session.recv_response(10);
    let locations = refs["result"].as_array().expect("an array of locations");
    assert_eq!(locations.len(), 2, "{refs}");
    assert_eq!(locations[0]["uri"], uri);

    // Inlay hints: the unannotated parameter reads as `a: Int`.
    session.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 11, "method": "textDocument/inlayHint",
        "params": {
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 0, "character": 0 },
                "end":   { "line": 3, "character": 0 }
            }
        }
    }));
    let hints = session.recv_response(11);
    let hints = hints["result"].as_array().expect("an array of hints");
    assert!(
        hints.iter().any(|h| h["label"] == ": Int"),
        "a parameter's inferred type: {hints:?}"
    );

    // Rename: an edit per reference, in this file.
    session.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 12, "method": "textDocument/rename",
        "params": {
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": 4 },
            "newName": "sum"
        }
    }));
    let renamed = session.recv_response(12);
    let edits = renamed["result"]["changes"][uri]
        .as_array()
        .expect("edits for this file");
    assert_eq!(edits.len(), 2, "{renamed}");
    assert_eq!(edits[0]["newText"], "sum");

    // …and a refused rename is a request **error**, so the client shows why.
    session.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 13, "method": "textDocument/rename",
        "params": {
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": 4 },
            "newName": "out"
        }
    }));
    let refused = session.recv_response(13);
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("out")),
        "a refusal names the collision: {refused}"
    );

    // Workspace symbols: with no root, the open documents are the workspace.
    session.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 14, "method": "workspace/symbol",
        "params": { "query": "foo" }
    }));
    let symbols = session.recv_response(14);
    let symbols = symbols["result"].as_array().expect("an array of symbols");
    assert_eq!(symbols.len(), 1, "{symbols:?}");
    assert_eq!(symbols[0]["name"], "foo");

    shutdown(&mut session);
    assert_eq!(session.finish(), 0);
}

/// A code action carries an edit a client can apply, over the wire.
#[test]
fn a_code_action_answers_with_an_applicable_edit() {
    let mut session = Session::start();
    initialize(&mut session);

    let uri = "file:///fix.px";
    session.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": { "textDocument": {
            "uri": uri, "languageId": "praxis", "version": 1,
            "text": "var v = read line(int)\nout(v.len())\n"
        }}
    }));

    session.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 20, "method": "textDocument/codeAction",
        "params": {
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 0, "character": 13 },
                "end":   { "line": 0, "character": 13 }
            },
            "context": { "diagnostics": [] }
        }
    }));
    let actions = session.recv_response(20);
    let actions = actions["result"].as_array().expect("an array of actions");
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert_eq!(actions[0]["kind"], "quickfix");
    assert!(
        actions[0]["title"]
            .as_str()
            .is_some_and(|t| t.contains("lines")),
        "§15.3's own example: {actions:?}"
    );
    let edit = &actions[0]["edit"]["changes"][uri][0];
    assert_eq!(edit["newText"], "lines");

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
/// `vscode-languageclient` does it whenever `TransportKind.stdio` is set. A CLI
/// that rejected the flag would exit 2 before a byte of protocol, and the
/// client would report it as a crash.
///
/// This does not depend on our own extension's argv being right, because the
/// next client to pass the flag will not be ours.
#[test]
fn the_server_accepts_the_stdio_flag_clients_append() {
    let mut session = Session::start_with(&["--stdio"]);
    let response = initialize(&mut session);
    assert_eq!(response["result"]["serverInfo"]["name"], "praxis-lsp");
    shutdown(&mut session);
    assert_eq!(session.finish(), 0);
}

/// A brace that opens a **block** does not open a menu; one that opens a
/// **record literal** does.
///
/// `{` is registered as a trigger character for the parser sublanguage, where
/// `` `{n:int}` `` needs a menu over text that is not yet an expression. The
/// editor fires it wherever the character is typed, so without this gate every
/// `fn f() {` and every `if x {` would pop the whole lexical list —
/// pre-selected, over an empty prefix, at the moment the user is about to type
/// a name that is by definition not in it — and the next <kbd>Enter</kbd> would
/// commit its first row.
///
/// This has to be a wire test: what distinguishes the two cases is
/// `params.context`, which the query layer never sees. And the second half is
/// the half that keeps the gate honest — simply dropping `{` from the trigger
/// list would pass the first assertion and lose `P {` → `x`, `y`.
#[test]
fn a_trigger_character_opens_a_menu_only_where_it_means_something() {
    let mut session = Session::start();
    initialize(&mut session);

    let uri = "file:///trigger.px";
    session.send(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": { "textDocument": {
            "uri": uri, "languageId": "praxis", "version": 1,
            "text": "struct P { x: Int, y: Int }\nfn main() -> Unit {\n  var p = P {}\n}\n"
        }}
    }));

    let brace_triggered = |line: u32, character: u32, id: i64| {
        serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "textDocument/completion",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
                "context": { "triggerKind": 2, "triggerCharacter": "{" }
            }
        })
    };

    // Just after the `{` of `fn main() -> Unit {`.
    session.send(&brace_triggered(1, 19, 30));
    let block = session.recv_response(30);
    let items = block["result"].as_array().expect("an item array");
    assert!(
        items.is_empty(),
        "a block's `{{` must not open a menu, got {} items: {block}",
        items.len()
    );

    // Just after the `{` of `P {}` — a record literal's own field names.
    session.send(&brace_triggered(2, 13, 31));
    let record = session.recv_response(31);
    let labels: Vec<&str> = record["result"]
        .as_array()
        .expect("an item array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"x") && labels.contains(&"y"),
        "a record literal's `{{` still offers its fields, got {labels:?}"
    );

    // The same block position, asked for rather than fired at: `INVOKED` is a
    // request and is never gated.
    session.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 32, "method": "textDocument/completion",
        "params": {
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": 19 },
            "context": { "triggerKind": 1 }
        }
    }));
    let invoked = session.recv_response(32);
    let labels: Vec<&str> = invoked["result"]
        .as_array()
        .expect("an item array")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"main"),
        "Ctrl+Space at the same offset still answers, got {labels:?}"
    );

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
