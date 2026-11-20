//! The MCP server an agent adds its findings through: `add_review_comment`
//! for a draft comment and `suggest_review` for a suggested verdict and
//! summary, over MCP's streamable HTTP transport with plain JSON responses
//! (`initialize`, `tools/list`, `tools/call`, notifications).
//!
//! Anything on this computer can connect to a local port, and over SSH the
//! forwarded port is open to every user of the remote machine, so:
//! - it listens on `127.0.0.1` only, and refuses a request a browser could
//!   send (an `Origin` header, a `Host` that is not a loopback name);
//! - each launch gets a 128-bit random token, the last part of its URL, that
//!   names one pull request; tokens are compared in constant time and an
//!   unknown one is a 404; a token dies with its terminal;
//! - request bodies, comment bodies, connections and comments per token are
//!   capped, and every argument is untrusted: the agent read the pull
//!   request, whose author can tell it anything. Paths and lines are checked
//!   against the loaded diff by the handler, and nothing is posted: a call
//!   makes a draft, or fills in the Submit review form, for the user to
//!   read first.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use forge::model::Side;
use serde_json::{Map, Value, json};

use crate::model::PrKey;
use crate::sink::Verdict;
use crate::untrusted::strip_controls;

/// The name the agent's config gives the server; its tools are
/// `mcp__review__<tool>` in Claude Code's permission rules.
pub const SERVER_NAME: &str = "review";
/// The tool that adds a draft comment.
pub const TOOL: &str = "add_review_comment";
/// The tool that suggests the review's verdict and summary.
pub const SUGGEST_TOOL: &str = "suggest_review";
/// What `--allowedTools` names so both tools run without a prompt.
pub const TOOL_RULES: &str = "mcp__review__add_review_comment,mcp__review__suggest_review";

/// The MCP versions this server speaks, newest first.
const PROTOCOLS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// The request line and headers.
const MAX_HEAD: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;
/// Longest path argument, in bytes.
const MAX_PATH: usize = 1024;
/// How long a connection may take to send its request or read the answer.
const IO_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a whole request may take to arrive, however it trickles in.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// What one server allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Bytes of one request body.
    pub request: usize,
    /// Bytes of one comment's body, or of a suggested summary.
    pub body: usize,
    /// Comments one token may add.
    pub comments: u32,
    /// Tool calls one token may make, added or not.
    pub calls: u32,
    /// Connections served at once.
    pub connections: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            request: 256 * 1024,
            body: 64 * 1024,
            comments: 300,
            calls: 1000,
            connections: 16,
        }
    }
}

/// One `add_review_comment` call, its arguments checked for shape: the
/// handler still checks them against the diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommentRequest {
    /// Forward slashes, no leading `./`, no control characters.
    pub path: String,
    /// 1-based, on `side`.
    pub line: u32,
    /// The first line of a range ending at `line`, below it.
    pub start_line: Option<u32>,
    pub side: Side,
    /// Markdown, without control characters other than line breaks and
    /// tabs; not empty.
    pub body: String,
}

/// One `suggest_review` call, its arguments checked. A later call replaces
/// an earlier one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuggestRequest {
    pub verdict: Verdict,
    /// Markdown for the review's summary, without control characters other
    /// than line breaks and tabs; may be empty.
    pub body: String,
}

/// A checked tool call, for the handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolRequest {
    Comment(CommentRequest),
    Suggest(SuggestRequest),
}

/// Turns a checked call into a draft or a suggestion, or says why not.
/// Called on the connection's thread; it may block while the app does the
/// work.
pub type Handler = Arc<dyn Fn(&PrKey, ToolRequest) -> Result<String, String> + Send + Sync>;

/// A per-launch secret: 128 random bits as 32 hex digits.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// # Errors
    /// When the system has no randomness to give.
    pub fn new() -> io::Result<Self> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Self(bytes.iter().map(|b| format!("{b:02x}")).collect()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

struct Grant {
    token: Token,
    key: PrKey,
    added: u32,
    calls: u32,
}

type Grants = Arc<Mutex<Vec<Grant>>>;

fn lock(grants: &Grants) -> MutexGuard<'_, Vec<Grant>> {
    grants.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The server: a thread that accepts on a loopback port, and one thread per
/// connection. Dropping it stops the listener.
pub struct McpServer {
    port: u16,
    grants: Grants,
    stop: Arc<AtomicBool>,
}

impl McpServer {
    /// Listens on a free port of `127.0.0.1`.
    ///
    /// # Errors
    /// When no port can be bound or the thread cannot start.
    pub fn start(handler: Handler, limits: Limits) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let grants: Grants = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Shared {
            grants: Arc::clone(&grants),
            handler,
            limits,
            active: Arc::new(AtomicUsize::new(0)),
        };
        let stopping = Arc::clone(&stop);
        thread::Builder::new()
            .name("review-mcp".to_owned())
            .spawn(move || accept(&listener, &stopping, &shared))?;
        Ok(Self { port, grants, stop })
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// A new token for `key`.
    ///
    /// # Errors
    /// When the system has no randomness to give.
    pub fn issue(&self, key: PrKey) -> io::Result<Token> {
        let token = Token::new()?;
        lock(&self.grants).push(Grant {
            token: token.clone(),
            key,
            added: 0,
            calls: 0,
        });
        Ok(token)
    }

    /// Ends `token`: its URL answers 404 from now on.
    pub fn revoke(&self, token: &Token) {
        lock(&self.grants).retain(|grant| grant.token != *token);
    }

    /// The URL for `token` as seen from `port` (the forwarded one over SSH).
    #[must_use]
    pub fn url(port: u16, token: &Token) -> String {
        format!("http://127.0.0.1:{port}/mcp/{}", token.as_str())
    }

    /// The `--mcp-config` text that names this server for `token`.
    #[must_use]
    pub fn config_json(port: u16, token: &Token) -> String {
        json!({
            "mcpServers": {
                SERVER_NAME: { "type": "http", "url": Self::url(port, token) }
            }
        })
        .to_string()
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accept loop so it sees the flag.
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port));
    }
}

#[derive(Clone)]
struct Shared {
    grants: Grants,
    handler: Handler,
    limits: Limits,
    active: Arc<AtomicUsize>,
}

fn accept(listener: &TcpListener, stop: &AtomicBool, shared: &Shared) {
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = stream else {
            continue;
        };
        let loopback = stream.peer_addr().is_ok_and(|peer| peer.ip().is_loopback());
        if !loopback {
            continue;
        }
        if shared.active.fetch_add(1, Ordering::SeqCst) >= shared.limits.connections {
            shared.active.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let conn = shared.clone();
        let spawned = thread::Builder::new()
            .name("review-mcp-conn".to_owned())
            .spawn(move || {
                serve(stream, &conn);
                conn.active.fetch_sub(1, Ordering::SeqCst);
            });
        if spawned.is_err() {
            // The closure, and its decrement, never ran.
            shared.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// An answer: a status and, for a JSON-RPC response, its body.
#[derive(Debug, PartialEq)]
struct Response {
    status: u16,
    body: Option<Value>,
}

impl Response {
    fn status(status: u16) -> Self {
        Self { status, body: None }
    }

    fn json(body: Value) -> Self {
        Self {
            status: 200,
            body: Some(body),
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        _ => "Error",
    }
}

fn serve(mut stream: TcpStream, shared: &Shared) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let response = match read_head(&mut stream, deadline) {
        Ok(head) => answer(head, &mut stream, deadline, shared),
        Err(status) => Response::status(status),
    };
    let body = response
        .body
        .map(|body| body.to_string().into_bytes())
        .unwrap_or_default();
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nConnection: close\r\nCache-Control: no-store\r\nContent-Length: {}\r\n",
        response.status,
        reason(response.status),
        body.len()
    );
    if response.status == 405 {
        out.push_str("Allow: POST\r\n");
    }
    if !body.is_empty() {
        out.push_str("Content-Type: application/json\r\n");
    }
    out.push_str("\r\n");
    let _ = stream.write_all(out.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

/// The request line and headers, and the bytes read past them.
struct Head {
    method: String,
    target: String,
    /// Names lowercase.
    headers: Vec<(String, String)>,
    rest: Vec<u8>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.as_str())
    }
}

fn read_head(stream: &mut impl Read, deadline: Instant) -> Result<Head, u16> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(at) = find(&buf, b"\r\n\r\n") {
            break at;
        }
        if buf.len() > MAX_HEAD {
            return Err(431);
        }
        if Instant::now() > deadline {
            return Err(408);
        }
        let n = stream.read(&mut chunk).map_err(|_| 400u16)?;
        if n == 0 {
            return Err(400);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| 400u16)?;
    let mut lines = head.split("\r\n");
    let mut request = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target), Some(version), None) = (
        request.next(),
        request.next(),
        request.next(),
        request.next(),
    ) else {
        return Err(400);
    };
    if !version.starts_with("HTTP/1.") {
        return Err(400);
    }
    let mut headers = Vec::new();
    for line in lines {
        if headers.len() == MAX_HEADERS {
            return Err(431);
        }
        let (name, value) = line.split_once(':').ok_or(400u16)?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    Ok(Head {
        method: method.to_owned(),
        target: target.to_owned(),
        headers,
        rest: buf[end + 4..].to_vec(),
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Whether two byte strings are equal, taking as long whichever byte differs.
fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// The grant whose token `candidate` is; every token is compared.
fn grant_of(grants: &[Grant], candidate: &str) -> Option<usize> {
    let mut found = None;
    for (ix, grant) in grants.iter().enumerate() {
        if same(grant.token.as_str().as_bytes(), candidate.as_bytes()) {
            found = Some(ix);
        }
    }
    found
}

/// Whether a `Host` header names this computer's loopback, as a client of
/// the port (or of its SSH forward) sends it; a page that rebinds a domain
/// to 127.0.0.1 sends its own name.
fn loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map_or(rest, |(name, _)| name),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

fn answer(head: Head, stream: &mut impl Read, deadline: Instant, shared: &Shared) -> Response {
    let Some(candidate) = head.target.strip_prefix("/mcp/") else {
        return Response::status(404);
    };
    let token = {
        let grants = lock(&shared.grants);
        match grant_of(&grants, candidate) {
            Some(ix) => grants[ix].token.clone(),
            None => return Response::status(404),
        }
    };
    if head.method != "POST" {
        return Response::status(405);
    }
    if head.header("origin").is_some() || !head.header("host").is_some_and(loopback_host) {
        return Response::status(403);
    }
    if head.header("transfer-encoding").is_some() {
        return Response::status(501);
    }
    let Some(length) = head.header("content-length") else {
        return Response::status(411);
    };
    let Ok(length) = length.parse::<usize>() else {
        return Response::status(400);
    };
    if length > shared.limits.request {
        return Response::status(413);
    }
    let mut body = head.rest;
    if body.len() > length {
        return Response::status(400);
    }
    body.reserve_exact(length - body.len());
    let mut chunk = [0u8; 8192];
    while body.len() < length {
        if Instant::now() > deadline {
            return Response::status(408);
        }
        let want = (length - body.len()).min(chunk.len());
        match stream.read(&mut chunk[..want]) {
            Ok(0) | Err(_) => return Response::status(400),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
        return Response::json(rpc_error(&Value::Null, -32700, "not JSON"));
    };
    match rpc(&message, &token, shared) {
        Some(reply) => Response::json(reply),
        None => Response::status(202),
    }
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// The reply to one JSON-RPC message; `None` for a notification or a reply
/// from the client, which take no answer.
fn rpc(message: &Value, token: &Token, shared: &Shared) -> Option<Value> {
    let Some(object) = message.as_object() else {
        return Some(rpc_error(&Value::Null, -32600, "one JSON-RPC object"));
    };
    let id = object.get("id")?;
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Some(rpc_error(id, -32600, "no method"));
    };
    let params = object.get("params");
    Some(match method {
        "initialize" => rpc_result(id, initialize(params)),
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({ "tools": [tool(), suggest_tool()] })),
        "tools/call" => rpc_result(id, call(params, token, shared)),
        _ => rpc_error(id, -32601, "unknown method"),
    })
}

fn initialize(params: Option<&Value>) -> Value {
    let asked = params
        .and_then(|params| params.get("protocolVersion"))
        .and_then(Value::as_str);
    let version = asked
        .and_then(|asked| PROTOCOLS.iter().find(|known| **known == asked))
        .unwrap_or(&PROTOCOLS[1]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Add each review finding with add_review_comment, then finish with \
            suggest_review: the verdict you recommend and a summary. Every comment becomes a \
            draft the user reads, edits or discards, and the suggestion only fills in the \
            Submit review form; the user decides, and nothing is posted by itself."
    })
}

fn tool() -> Value {
    json!({
        "name": TOOL,
        "description": "Adds a draft review comment on a line of the pull request's diff. \
            The user reads every draft before anything is posted. The line must be one the \
            diff shows: an added or unchanged line of the new file (side \"new\"), or a \
            removed line of the old file (side \"old\"), numbered as in that file.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "A file the pull request changes, relative to the repository root." },
                "line": { "type": "integer", "minimum": 1, "description": "The line the comment is on, 1-based." },
                "start_line": { "type": "integer", "minimum": 1, "description": "For a comment on a range: its first line, before `line`." },
                "side": { "type": "string", "enum": ["new", "old"], "description": "new (default): the file after the change; old: before it." },
                "body": { "type": "string", "description": "The comment, in Markdown." }
            },
            "required": ["path", "line", "body"]
        }
    })
}

fn suggest_tool() -> Value {
    json!({
        "name": SUGGEST_TOOL,
        "description": "Suggests the review's verdict and summary, once your comments are in. \
            It fills in the user's Submit review form, marked as from Claude (an approval is \
            only offered there, for the user to take), and never submits anything: the user decides whether to keep, change or discard it, and whether to \
            submit. Calling it again replaces the earlier suggestion.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "verdict": {
                    "type": "string",
                    "enum": ["comment", "approve", "request_changes"],
                    "description": "comment: feedback without a decision; approve: the changes can merge; request_changes: something must change first."
                },
                "body": { "type": "string", "description": "The review's summary, in Markdown; an empty string for none." }
            },
            "required": ["verdict", "body"]
        }
    })
}

fn tool_text(text: &str, error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": error })
}

/// Which tool a call names.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Comment,
    Suggest,
}

fn call(params: Option<&Value>, token: &Token, shared: &Shared) -> Value {
    let name = params
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str);
    let tool = match name {
        Some(TOOL) => Tool::Comment,
        Some(SUGGEST_TOOL) => Tool::Suggest,
        _ => return tool_text(&format!("The tools are {TOOL} and {SUGGEST_TOOL}."), true),
    };
    let key = {
        let mut grants = lock(&shared.grants);
        let Some(grant) = grants.iter_mut().find(|grant| grant.token == *token) else {
            return tool_text("This review has ended.", true);
        };
        grant.calls += 1;
        if grant.calls > shared.limits.calls {
            return tool_text(
                &format!(
                    "No more calls: a review makes at most {} of them.",
                    shared.limits.calls
                ),
                true,
            );
        }
        if tool == Tool::Comment && grant.added >= shared.limits.comments {
            return tool_text(
                &format!(
                    "No more comments: a review takes at most {} of them.",
                    shared.limits.comments
                ),
                true,
            );
        }
        grant.key.clone()
    };
    let arguments = params.and_then(|params| params.get("arguments"));
    let request = match tool {
        Tool::Comment => parse_comment(arguments, shared.limits.body).map(ToolRequest::Comment),
        Tool::Suggest => parse_suggestion(arguments, shared.limits.body).map(ToolRequest::Suggest),
    };
    let request = match request {
        Ok(request) => request,
        Err(why) => return tool_text(&why, true),
    };
    match (shared.handler)(&key, request) {
        Ok(text) => {
            if tool == Tool::Comment
                && let Some(grant) = lock(&shared.grants)
                    .iter_mut()
                    .find(|grant| grant.token == *token)
            {
                grant.added += 1;
            }
            tool_text(&text, false)
        }
        Err(why) => tool_text(&why, true),
    }
}

/// A call's arguments object; none is an empty one.
fn arguments_of(arguments: Option<&Value>) -> Result<Option<&Map<String, Value>>, String> {
    match arguments {
        None => Ok(None),
        Some(Value::Object(args)) => Ok(Some(args)),
        Some(_) => Err("The arguments must be an object.".to_owned()),
    }
}

/// `body` without control characters and trimmed, once it fits in `max`
/// bytes.
fn clean_body(body: &str, max: usize) -> Result<String, String> {
    if body.len() > max {
        return Err(format!(
            "The body is longer than {} KiB; shorten it.",
            max / 1024
        ));
    }
    Ok(strip_controls(body).trim().to_owned())
}

fn parse_suggestion(arguments: Option<&Value>, max_body: usize) -> Result<SuggestRequest, String> {
    let empty = Map::new();
    let args = arguments_of(arguments)?.unwrap_or(&empty);
    let verdict = match args.get("verdict").and_then(Value::as_str) {
        Some("comment") => Verdict::Comment,
        Some("approve") => Verdict::Approve,
        Some("request_changes") => Verdict::RequestChanges,
        _ => {
            return Err(
                "`verdict` is required: \"comment\", \"approve\" or \"request_changes\"."
                    .to_owned(),
            );
        }
    };
    let body = args
        .get("body")
        .and_then(Value::as_str)
        .ok_or("`body` is required: the review's summary, or an empty string for none.")?;
    Ok(SuggestRequest {
        verdict,
        body: clean_body(body, max_body)?,
    })
}

fn parse_comment(arguments: Option<&Value>, max_body: usize) -> Result<CommentRequest, String> {
    let empty = Map::new();
    let args = arguments_of(arguments)?.unwrap_or(&empty);
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or("`path` is required: a file the pull request changes.")?;
    if path.len() > MAX_PATH || path.chars().any(char::is_control) {
        return Err("`path` is not a usable file path.".to_owned());
    }
    let path = path.trim().trim_start_matches("./").replace('\\', "/");
    if path.is_empty() {
        return Err("`path` is empty.".to_owned());
    }
    let number = |name: &str| -> Result<Option<u32>, String> {
        match args.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n >= 1)
                .map(Some)
                .ok_or_else(|| format!("`{name}` must be a line number, 1 or more.")),
        }
    };
    let line = number("line")?.ok_or("`line` is required.")?;
    let start_line = number("start_line")?.filter(|start| *start != line);
    if start_line.is_some_and(|start| start > line) {
        return Err("`start_line` must come before `line`.".to_owned());
    }
    let side = match args.get("side").and_then(Value::as_str) {
        None => Side::New,
        Some(side) if side.eq_ignore_ascii_case("new") => Side::New,
        Some(side) if side.eq_ignore_ascii_case("old") => Side::Old,
        Some(_) => return Err("`side` is \"new\" or \"old\".".to_owned()),
    };
    let body = args
        .get("body")
        .and_then(Value::as_str)
        .ok_or("`body` is required.")?;
    let body = clean_body(body, max_body)?;
    if body.is_empty() {
        return Err("The body is empty.".to_owned());
    }
    Ok(CommentRequest {
        path,
        line,
        start_line,
        side,
        body,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{PrNumber, RepoRef};

    pub(crate) fn key() -> PrKey {
        PrKey::new(RepoRef::new("github.com", "o", "r"), PrNumber(4))
    }

    /// Sends `body` to `path` with `headers` and returns the status and the
    /// JSON answer.
    pub(crate) fn post(port: u16, path: &str, headers: &str, body: &str) -> (u16, Option<Value>) {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\n{headers}Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).expect("send");
        read_response(stream)
    }

    fn read_response(mut stream: TcpStream) -> (u16, Option<Value>) {
        let mut text = String::new();
        stream.read_to_string(&mut text).expect("read");
        let (head, body) = text.split_once("\r\n\r\n").expect("a response");
        let status = head
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("a status");
        (status, serde_json::from_str(body).ok())
    }

    fn raw(port: u16, request: &str) -> u16 {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect");
        stream.write_all(request.as_bytes()).expect("send");
        read_response(stream).0
    }

    type Calls = Arc<Mutex<Vec<(PrKey, ToolRequest)>>>;

    fn server(limits: Limits) -> (McpServer, Calls) {
        let calls: Calls = Arc::default();
        let seen = Arc::clone(&calls);
        let handler: Handler = Arc::new(move |key: &PrKey, request: ToolRequest| {
            let text = match &request {
                ToolRequest::Comment(comment) if comment.path != "src/a.rs" => {
                    return Err(format!("{} is not in the diff", comment.path));
                }
                ToolRequest::Comment(comment) => {
                    format!("added on {}:{}", comment.path, comment.line)
                }
                ToolRequest::Suggest(suggestion) => {
                    format!("suggested {}", suggestion.verdict.label())
                }
            };
            seen.lock().unwrap().push((key.clone(), request));
            Ok(text)
        });
        (McpServer::start(handler, limits).expect("start"), calls)
    }

    fn call_body(arguments: &Value) -> String {
        named_call(TOOL, arguments)
    }

    fn named_call(tool: &str, arguments: &Value) -> String {
        json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call",
                "params": { "name": tool, "arguments": arguments } })
        .to_string()
    }

    fn tool_result(reply: Option<Value>) -> (String, bool) {
        let reply = reply.expect("a reply");
        let result = &reply["result"];
        (
            result["content"][0]["text"].as_str().unwrap().to_owned(),
            result["isError"].as_bool().unwrap(),
        )
    }

    #[test]
    fn initialize_list_and_call_add_a_draft() {
        let (server, calls) = server(Limits::default());
        let token = server.issue(key()).unwrap();
        assert_eq!(token.as_str().len(), 32);
        let path = format!("/mcp/{}", token.as_str());
        let port = server.port();

        let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {},
                        "clientInfo": { "name": "claude-code", "version": "2" } } });
        let (status, reply) = post(port, &path, "", &init.to_string());
        assert_eq!(status, 200);
        let reply = reply.unwrap();
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(reply["result"]["serverInfo"]["name"], SERVER_NAME);
        assert!(reply["result"]["capabilities"]["tools"].is_object());

        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert_eq!(post(port, &path, "", &note.to_string()), (202, None));

        let list = json!({ "jsonrpc": "2.0", "id": "l", "method": "tools/list" });
        let (status, reply) = post(port, &path, "", &list.to_string());
        assert_eq!(status, 200);
        let tools = &reply.unwrap()["result"]["tools"];
        assert_eq!(tools[0]["name"], TOOL);
        assert_eq!(
            tools[0]["inputSchema"]["required"],
            json!(["path", "line", "body"])
        );

        let args = json!({ "path": "./src/a.rs", "line": 31, "side": "new",
                           "body": "Off by one.\u{1b}[2J\u{202e}" });
        let (status, reply) = post(port, &path, "", &call_body(&args));
        assert_eq!(status, 200);
        assert_eq!(
            tool_result(reply),
            ("added on src/a.rs:31".to_owned(), false)
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, key());
        assert_eq!(
            calls[0].1,
            ToolRequest::Comment(CommentRequest {
                path: "src/a.rs".into(),
                line: 31,
                start_line: None,
                side: Side::New,
                body: "Off by one.[2J".into(),
            })
        );
    }

    #[test]
    fn suggest_review_is_listed_and_passes_a_checked_suggestion() {
        let (server, calls) = server(Limits::default());
        let token = server.issue(key()).unwrap();
        let path = format!("/mcp/{}", token.as_str());
        let port = server.port();
        let list = json!({ "jsonrpc": "2.0", "id": "l", "method": "tools/list" });
        let tools = post(port, &path, "", &list.to_string()).1.unwrap()["result"]["tools"].clone();
        assert_eq!(tools[1]["name"], SUGGEST_TOOL);
        assert_eq!(
            tools[1]["inputSchema"]["required"],
            json!(["verdict", "body"])
        );
        assert_eq!(
            tools[1]["inputSchema"]["properties"]["verdict"]["enum"],
            json!(["comment", "approve", "request_changes"])
        );
        let described = tools[1]["description"].as_str().unwrap();
        assert!(described.contains("never submits"), "{described}");

        for (args, verdict, body) in [
            (
                json!({ "verdict": "approve", "body": "  Looks good.\u{1b}[2J\n\nShip it.\u{202e} " }),
                Verdict::Approve,
                "Looks good.[2J\n\nShip it.",
            ),
            (
                json!({ "verdict": "request_changes", "body": "Needs a test." }),
                Verdict::RequestChanges,
                "Needs a test.",
            ),
            (
                json!({ "verdict": "comment", "body": "" }),
                Verdict::Comment,
                "",
            ),
        ] {
            let (text, error) =
                tool_result(post(port, &path, "", &named_call(SUGGEST_TOOL, &args)).1);
            assert!(!error, "{args}: {text}");
            assert_eq!(text, format!("suggested {}", verdict.label()));
            let last = calls.lock().unwrap().last().cloned().unwrap();
            assert_eq!(
                last,
                (
                    key(),
                    ToolRequest::Suggest(SuggestRequest {
                        verdict,
                        body: body.to_owned(),
                    })
                )
            );
        }
    }

    #[test]
    fn bad_suggestions_are_tool_errors() {
        let (server, calls) = server(Limits::default());
        let token = server.issue(key()).unwrap();
        let path = format!("/mcp/{}", token.as_str());
        let long = "y".repeat(Limits::default().body + 1);
        for (args, says) in [
            (json!({ "body": "x" }), "`verdict` is required"),
            (json!({ "verdict": "lgtm", "body": "x" }), "`verdict`"),
            (json!({ "verdict": "Approve", "body": "x" }), "`verdict`"),
            (
                json!({ "verdict": "request-changes", "body": "x" }),
                "`verdict`",
            ),
            (json!({ "verdict": 1, "body": "x" }), "`verdict`"),
            (json!({ "verdict": "approve" }), "`body` is required"),
            (
                json!({ "verdict": "approve", "body": 3 }),
                "`body` is required",
            ),
            (json!({ "verdict": "approve", "body": long }), "64 KiB"),
            (json!(["approve", "x"]), "must be an object"),
        ] {
            let body = named_call(SUGGEST_TOOL, &args);
            let (text, error) = tool_result(post(server.port(), &path, "", &body).1);
            assert!(error, "{args}");
            assert!(text.contains(says), "{args}: {text}");
        }
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_suggestion_is_for_its_token_s_pull_request_only() {
        let limits = Limits {
            comments: 1,
            ..Limits::default()
        };
        let (server, calls) = server(limits);
        let other_key = PrKey::new(RepoRef::new("github.com", "o", "r"), PrNumber(5));
        let mine = server.issue(key()).unwrap();
        let theirs = server.issue(other_key.clone()).unwrap();
        let args = json!({ "verdict": "approve", "body": "x" });
        let send = |token: &str| {
            let path = format!("/mcp/{token}");
            post(server.port(), &path, "", &named_call(SUGGEST_TOOL, &args))
        };

        assert!(!tool_result(send(mine.as_str()).1).1);
        assert!(!tool_result(send(theirs.as_str()).1).1);
        let keys: Vec<PrKey> = calls
            .lock()
            .unwrap()
            .iter()
            .map(|(key, _)| key.clone())
            .collect();
        assert_eq!(
            keys,
            [key(), other_key],
            "each token names its own pull request"
        );

        // Suggestions do not use up the comment cap, and a spent cap does
        // not stop a suggestion.
        let comment = json!({ "path": "src/a.rs", "line": 3, "body": "x" });
        let path = format!("/mcp/{}", mine.as_str());
        assert!(!tool_result(post(server.port(), &path, "", &call_body(&comment)).1).1);
        assert!(tool_result(post(server.port(), &path, "", &call_body(&comment)).1).1);
        assert!(!tool_result(send(mine.as_str()).1).1);

        let mut wrong = mine.as_str().to_owned();
        wrong.replace_range(..1, if wrong.starts_with('0') { "1" } else { "0" });
        assert_eq!(send(&wrong).0, 404);
        server.revoke(&mine);
        assert_eq!(
            send(mine.as_str()).0,
            404,
            "a revoked token suggests nothing"
        );
        assert_eq!(calls.lock().unwrap().len(), 4);
    }

    #[test]
    fn bad_arguments_and_handler_refusals_are_tool_errors() {
        let (server, calls) = server(Limits::default());
        let token = server.issue(key()).unwrap();
        let path = format!("/mcp/{}", token.as_str());
        for (args, says) in [
            (json!({ "line": 3, "body": "x" }), "`path` is required"),
            (
                json!({ "path": "src/a.rs", "line": 0, "body": "x" }),
                "`line` must be",
            ),
            (
                json!({ "path": "src/a.rs", "line": -2, "body": "x" }),
                "`line` must be",
            ),
            (
                json!({ "path": "src/a.rs", "line": 3, "body": " \u{7} " }),
                "empty",
            ),
            (
                json!({ "path": "src/a.rs", "line": 3, "side": "both", "body": "x" }),
                "`side`",
            ),
            (
                json!({ "path": "src/a.rs", "line": 3, "start_line": 9, "body": "x" }),
                "before",
            ),
            (
                json!({ "path": "src/\u{0}a.rs", "line": 3, "body": "x" }),
                "usable",
            ),
            (
                json!({ "path": "src/b.rs", "line": 3, "body": "x" }),
                "not in the diff",
            ),
        ] {
            let (text, error) = tool_result(post(server.port(), &path, "", &call_body(&args)).1);
            assert!(error, "{args}");
            assert!(text.contains(says), "{args}: {text}");
        }
        let long = "y".repeat(Limits::default().body + 1);
        let args = json!({ "path": "src/a.rs", "line": 3, "body": long });
        let (text, error) = tool_result(post(server.port(), &path, "", &call_body(&args)).1);
        assert!(error && text.contains("64 KiB"), "{text}");
        assert!(calls.lock().unwrap().is_empty());

        let other = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                            "params": { "name": "run_shell", "arguments": {} } });
        let (text, error) = tool_result(post(server.port(), &path, "", &other.to_string()).1);
        assert!(error && text.contains(TOOL) && text.contains(SUGGEST_TOOL));
        let unknown = json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list" });
        let reply = post(server.port(), &path, "", &unknown.to_string())
            .1
            .unwrap();
        assert_eq!(reply["error"]["code"], -32601);
    }

    #[test]
    fn unknown_tokens_browsers_and_big_bodies_are_turned_away() {
        let (server, calls) = server(Limits::default());
        let token = server.issue(key()).unwrap();
        let port = server.port();
        let path = format!("/mcp/{}", token.as_str());
        let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string();
        assert_eq!(post(port, &path, "", &ping).0, 200);

        let mut wrong = token.as_str().to_owned();
        wrong.replace_range(..1, if wrong.starts_with('0') { "1" } else { "0" });
        for bad in [
            format!("/mcp/{wrong}"),
            format!("/mcp/{}", &token.as_str()[..31]),
            format!("/mcp/{}x", token.as_str()),
            "/mcp/".to_owned(),
            "/".to_owned(),
            format!("/other/{}", token.as_str()),
        ] {
            assert_eq!(post(port, &bad, "", &ping).0, 404, "{bad}");
        }
        assert_eq!(
            post(port, &path, "Origin: https://evil.example\r\n", &ping).0,
            403
        );
        let rebound = format!(
            "POST {path} HTTP/1.1\r\nHost: evil.example:{port}\r\nContent-Length: {}\r\n\r\n{ping}",
            ping.len()
        );
        assert_eq!(raw(port, &rebound), 403);
        let get = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert_eq!(raw(port, &get), 405);

        // Turned away on the length alone, before the body is read.
        let big = Limits::default().request + 1;
        let oversized = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {big}\r\n\r\n"
        );
        assert_eq!(raw(port, &oversized), 413);
        let chunked = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\n\r\n"
        );
        assert_eq!(raw(port, &chunked), 501);
        let no_length = format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert_eq!(raw(port, &no_length), 411);

        server.revoke(&token);
        assert_eq!(
            post(port, &path, "", &ping).0,
            404,
            "a revoked token is gone"
        );
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_token_adds_at_most_its_cap_of_comments() {
        let limits = Limits {
            comments: 3,
            ..Limits::default()
        };
        let (server, calls) = server(limits);
        let token = server.issue(key()).unwrap();
        let other = server.issue(key()).unwrap();
        let args = json!({ "path": "src/a.rs", "line": 3, "body": "x" });
        let send = |token: &Token| {
            let path = format!("/mcp/{}", token.as_str());
            tool_result(post(server.port(), &path, "", &call_body(&args)).1)
        };
        for _ in 0..3 {
            assert!(!send(&token).1);
        }
        let (text, error) = send(&token);
        assert!(error && text.contains("at most 3"), "{text}");
        assert!(!send(&other).1, "another launch has its own count");
        assert_eq!(calls.lock().unwrap().len(), 4);
        assert_eq!(Limits::default().comments, 300);
        assert_eq!(Limits::default().request, 256 * 1024);
        assert_eq!(Limits::default().body, 64 * 1024);
    }

    #[test]
    fn the_config_names_the_loopback_url() {
        let token = Token::new().unwrap();
        let config: Value = serde_json::from_str(&McpServer::config_json(4321, &token)).unwrap();
        let server = &config["mcpServers"][SERVER_NAME];
        assert_eq!(server["type"], "http");
        assert_eq!(
            server["url"],
            format!("http://127.0.0.1:4321/mcp/{}", token.as_str())
        );
        assert_eq!(format!("{token:?}"), "Token(..)", "never printed");
        assert_ne!(Token::new().unwrap(), token);
        assert!(
            loopback_host("127.0.0.1:80") && loopback_host("localhost") && loopback_host("[::1]:9")
        );
        assert!(!loopback_host("127.0.0.1.evil.example:80") && !loopback_host("evil"));
    }
}
