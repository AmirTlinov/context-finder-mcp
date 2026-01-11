use base64::Engine as _;
use rmcp::model::{JsonRpcMessage, RequestId};
use rmcp::service::{RxJsonRpcMessage, ServiceRole, TxJsonRpcMessage};
use rmcp::transport::Transport;
use serde::Serialize;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::io;
use std::io::Write as _;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Framing {
    Unknown,
    NewlineJson,
    ContentLength,
}

const MAX_BUFFER_BYTES: usize = if cfg!(test) { 4096 } else { 32 * 1024 * 1024 };
const MAX_MESSAGE_BYTES: usize = if cfg!(test) { 1024 } else { 16 * 1024 * 1024 };

fn sanitize_json_schema_value(schema: &mut Value) {
    // The MCP spec uses JSON Schema. Some generators (and OpenAPI-like adapters) emit `nullable`,
    // which is not part of JSON Schema draft-07 and can cause strict clients to reject the tool
    // surface (closing the transport).
    //
    // Agent-native policy: keep tool schemas strictly draft-07-compatible, but keep the runtime
    // tolerant of nulls (serde Option<T> already accepts null as None).
    match schema {
        Value::Array(items) => {
            for item in items {
                sanitize_json_schema_value(item);
            }
        }
        Value::Object(map) => {
            // Keep schemas compact: many MCP clients request tools/list on every session start,
            // and large schema payloads can trip conservative transport limits.
            map.remove("title");
            map.remove("description");

            let nullable = matches!(map.get("nullable"), Some(Value::Bool(true)));
            if nullable {
                map.remove("nullable");

                // Common OpenAPI-style null branch: `{ "const": null, "nullable": true }`.
                if matches!(map.get("const"), Some(Value::Null)) && !map.contains_key("type") {
                    map.remove("const");
                    map.insert("type".to_string(), Value::String("null".to_string()));
                } else if let Some(type_value) = map.get_mut("type") {
                    match type_value {
                        Value::String(t) => {
                            if t != "null" {
                                *type_value = Value::Array(vec![
                                    Value::String(t.clone()),
                                    Value::String("null".to_string()),
                                ]);
                            }
                        }
                        Value::Array(types) => {
                            let has_null = types
                                .iter()
                                .any(|v| v.as_str().is_some_and(|s| s == "null"));
                            if !has_null {
                                types.push(Value::String("null".to_string()));
                            }
                        }
                        _ => {}
                    }
                }
            }

            // `format` is an annotation in draft-07, but some strict validators treat unknown
            // formats as an error. Keep only widely-supported formats.
            if let Some(format_value) = map.get("format").and_then(Value::as_str) {
                let keep = matches!(
                    format_value,
                    "date-time"
                        | "date"
                        | "time"
                        | "email"
                        | "hostname"
                        | "ipv4"
                        | "ipv6"
                        | "uri"
                        | "uuid"
                );
                if !keep {
                    map.remove("format");
                }
            }

            for value in map.values_mut() {
                sanitize_json_schema_value(value);
            }
        }
        _ => {}
    }
}

fn sanitize_tools_list_schema(message: &mut Value) {
    let Some(result) = message.get_mut("result") else {
        return;
    };
    let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools {
        if let Some(schema) = tool.get_mut("inputSchema") {
            sanitize_json_schema_value(schema);
        }
    }
}

const fn is_ascii_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug)]
struct FrameDump {
    file: std::fs::File,
}

#[derive(Serialize)]
struct FrameDumpLine<'a> {
    ts_ms: u64,
    dir: &'a str,
    len: usize,
    b64: String,
}

fn frame_dump_from_env() -> Option<Arc<Mutex<FrameDump>>> {
    let raw_path = std::env::var("CONTEXT_MCP_DUMP_FRAMES")
        .or_else(|_| std::env::var("CONTEXT_FINDER_MCP_DUMP_FRAMES"))
        .ok()?;
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return None;
    }

    let path = PathBuf::from(trimmed);
    let final_path = if path.is_dir() {
        path.join(format!("context_mcp_frames_{}.jsonl", std::process::id()))
    } else {
        path
    };

    if let Some(parent) = final_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&final_path)
        .ok()?;

    let mut dump = FrameDump { file };
    // Best-effort "start" marker for debugging harness integrations.
    let _ = writeln!(
        dump.file,
        "{}",
        serde_json::json!({
            "ts_ms": now_unix_ms(),
            "event": "start",
            "pid": std::process::id(),
            "exe": std::env::current_exe().ok().map(|p| p.to_string_lossy().to_string()),
        })
    );
    Some(Arc::new(Mutex::new(dump)))
}

fn strip_leading_whitespace(buf: &mut Vec<u8>) {
    let first_non_ws = buf.iter().position(|b| !is_ascii_whitespace(*b));
    match first_non_ws {
        None => buf.clear(),
        Some(0) => {}
        Some(n) => {
            buf.drain(..n);
        }
    }
}

fn strip_utf8_bom(buf: &mut Vec<u8>) {
    const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
    if buf.starts_with(BOM) {
        buf.drain(..BOM.len());
    }
}

fn starts_with_content_length(buf: &[u8]) -> bool {
    const PREFIX: &[u8] = b"content-length:";
    if buf.len() < PREFIX.len() {
        return false;
    }
    buf[..PREFIX.len()].eq_ignore_ascii_case(PREFIX)
}

fn first_non_ws_byte(buf: &[u8]) -> Option<(usize, u8)> {
    let start = buf.iter().position(|b| !is_ascii_whitespace(*b))?;
    Some((start, buf[start]))
}

fn find_double_newline(buf: &[u8]) -> Option<(usize, usize)> {
    // Returns: (header_end_index, newline_width)
    // Prefer CRLFCRLF, fall back to LFLF.
    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some((pos + 4, 4));
    }
    if let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
        return Some((pos + 2, 2));
    }
    None
}

fn parse_content_length(headers: &str) -> Option<usize> {
    for raw_line in headers.lines() {
        let line = raw_line.trim_end_matches('\r').trim();
        if line.len() < "content-length:".len() {
            continue;
        }
        if line.as_bytes()[.."content-length:".len()].eq_ignore_ascii_case(b"content-length:") {
            let value = line["content-length:".len()..].trim();
            if let Ok(n) = value.parse::<usize>() {
                return Some(n);
            }
        }
    }
    None
}

/// Hybrid stdio transport that supports both:
/// - newline-delimited JSON-RPC (one JSON object per line)
/// - LSP-style `Content-Length: N\r\n\r\n<json>` framing
///
/// It auto-detects framing from the first non-whitespace bytes received.
pub struct HybridStdioTransport<Role: ServiceRole, R: AsyncRead, W: AsyncWrite> {
    read: R,
    write_tx: Option<mpsc::Sender<WriteRequest>>,
    write_task: Option<tokio::task::JoinHandle<()>>,
    buf: Vec<u8>,
    pending: VecDeque<RxJsonRpcMessage<Role>>,
    framing: Framing,
    dump: Option<Arc<Mutex<FrameDump>>>,
    _marker: PhantomData<fn() -> (Role, W)>,
}

struct WriteRequest {
    bytes: Vec<u8>,
    reply: oneshot::Sender<io::Result<()>>,
}

async fn run_write_loop<W: AsyncWrite + Unpin>(
    mut write: W,
    mut rx: mpsc::Receiver<WriteRequest>,
    dump: Option<Arc<Mutex<FrameDump>>>,
) {
    while let Some(req) = rx.recv().await {
        if let Some(dump) = dump.as_ref() {
            if let Ok(mut guard) = dump.lock() {
                let line = FrameDumpLine {
                    ts_ms: now_unix_ms(),
                    dir: "tx",
                    len: req.bytes.len(),
                    b64: base64::engine::general_purpose::STANDARD.encode(&req.bytes),
                };
                if let Ok(payload) = serde_json::to_string(&line) {
                    let _ = writeln!(guard.file, "{payload}");
                }
            }
        }
        let result = async {
            write.write_all(&req.bytes).await?;
            write.flush().await?;
            Ok(())
        }
        .await;
        let should_stop = result.is_err();
        let _ = req.reply.send(result);
        if should_stop {
            break;
        }
    }
}

impl<Role: ServiceRole, R: AsyncRead + Unpin + Send, W: AsyncWrite + Unpin + Send + 'static>
    HybridStdioTransport<Role, R, W>
{
    pub fn new(read: R, write: W) -> Self {
        let dump = frame_dump_from_env();
        let (write_tx, write_rx) = mpsc::channel::<WriteRequest>(16);
        let write_task = tokio::spawn(run_write_loop(write, write_rx, dump.clone()));
        Self {
            read,
            write_tx: Some(write_tx),
            write_task: Some(write_task),
            buf: Vec::new(),
            pending: VecDeque::new(),
            framing: Framing::Unknown,
            dump,
            _marker: PhantomData,
        }
    }

    fn queue_batch(
        &mut self,
        messages: Vec<RxJsonRpcMessage<Role>>,
    ) -> Option<RxJsonRpcMessage<Role>> {
        let mut it = messages.into_iter();
        let first = it.next()?;
        self.pending.extend(it);
        Some(first)
    }

    fn detect_framing(&mut self) {
        if self.framing != Framing::Unknown {
            return;
        }
        strip_utf8_bom(&mut self.buf);
        strip_leading_whitespace(&mut self.buf);
        if self.buf.is_empty() {
            return;
        }
        if starts_with_content_length(&self.buf) {
            self.framing = Framing::ContentLength;
            return;
        }
        // Heuristic: compact JSON messages start with '{' or '['.
        if matches!(self.buf[0], b'{' | b'[') {
            self.framing = Framing::NewlineJson;
            return;
        }
        // Fallback: treat as newline JSON; we will still tolerate garbage lines.
        self.framing = Framing::NewlineJson;
    }

    fn try_decode_newline(&mut self) -> Result<Option<RxJsonRpcMessage<Role>>, io::Error> {
        loop {
            // Be more permissive than "one JSON per line": some clients pretty-print JSON-RPC
            // messages (multi-line) while still using newline framing. In that case we must parse
            // from the raw buffer as a JSON stream, not line-by-line.
            //
            // This keeps the transport agent-native: we should not crash a session just because
            // the client used `to_string_pretty()` instead of compact JSON.
            if let Some((start, first)) = first_non_ws_byte(&self.buf) {
                let slice = &self.buf[start..];
                if starts_with_content_length(slice) {
                    self.framing = Framing::ContentLength;
                    return self.try_decode();
                }
                if matches!(first, b'{' | b'[') {
                    if first == b'[' {
                        let mut stream =
                            serde_json::Deserializer::from_slice(slice).into_iter::<Value>();
                        match stream.next() {
                            Some(Ok(value)) => {
                                let used = start.saturating_add(stream.byte_offset());
                                self.buf.drain(..used);
                                let Some(items) = value.as_array() else {
                                    return Err(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "invalid JSON-RPC batch (expected array)",
                                    ));
                                };
                                let mut batch = Vec::with_capacity(items.len());
                                for item in items {
                                    let msg = serde_json::from_value::<RxJsonRpcMessage<Role>>(
                                        item.clone(),
                                    )
                                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                                    batch.push(msg);
                                }
                                return Ok(self.queue_batch(batch));
                            }
                            Some(Err(err)) if err.is_eof() => return Ok(None),
                            Some(Err(err)) => {
                                return Err(io::Error::new(io::ErrorKind::InvalidData, err))
                            }
                            None => return Ok(None),
                        }
                    }

                    let mut stream = serde_json::Deserializer::from_slice(slice)
                        .into_iter::<RxJsonRpcMessage<Role>>();
                    match stream.next() {
                        Some(Ok(msg)) => {
                            let used = start.saturating_add(stream.byte_offset());
                            self.buf.drain(..used);
                            return Ok(Some(msg));
                        }
                        Some(Err(err)) if err.is_eof() => return Ok(None),
                        Some(Err(err)) => {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, err));
                        }
                        None => return Ok(None),
                    }
                }
            }

            let Some(nl) = self.buf.iter().position(|b| *b == b'\n') else {
                // Compat: some clients write raw JSON to stdin without newline delimiters.
                // Try to parse a complete JSON-RPC message from the buffer.
                let start = match first_non_ws_byte(&self.buf).map(|(idx, _)| idx) {
                    Some(pos) => pos,
                    None => return Ok(None),
                };
                let slice = &self.buf[start..];

                // If we see a Content-Length header while in newline mode, switch modes.
                if starts_with_content_length(slice) {
                    self.framing = Framing::ContentLength;
                    return self.try_decode();
                }

                if matches!(slice.first(), Some(b'[')) {
                    let mut stream =
                        serde_json::Deserializer::from_slice(slice).into_iter::<Value>();
                    match stream.next() {
                        Some(Ok(value)) => {
                            let used = start.saturating_add(stream.byte_offset());
                            self.buf.drain(..used);
                            let Some(items) = value.as_array() else {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "invalid JSON-RPC batch (expected array)",
                                ));
                            };
                            let mut batch = Vec::with_capacity(items.len());
                            for item in items {
                                let msg =
                                    serde_json::from_value::<RxJsonRpcMessage<Role>>(item.clone())
                                        .map_err(|e| {
                                            io::Error::new(io::ErrorKind::InvalidData, e)
                                        })?;
                                batch.push(msg);
                            }
                            return Ok(self.queue_batch(batch));
                        }
                        Some(Err(err)) if err.is_eof() => return Ok(None),
                        Some(Err(err)) => {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, err))
                        }
                        None => return Ok(None),
                    }
                }

                let mut stream = serde_json::Deserializer::from_slice(slice)
                    .into_iter::<RxJsonRpcMessage<Role>>();
                match stream.next() {
                    Some(Ok(msg)) => {
                        let used = start.saturating_add(stream.byte_offset());
                        self.buf.drain(..used);
                        return Ok(Some(msg));
                    }
                    Some(Err(err)) if err.is_eof() => return Ok(None),
                    Some(Err(err)) => {
                        // In newline mode we tolerate garbage *lines*, but without a newline we
                        // cannot safely recover.
                        return Err(io::Error::new(io::ErrorKind::InvalidData, err));
                    }
                    None => return Ok(None),
                }
            };
            let mut line = self.buf.drain(..=nl).collect::<Vec<u8>>();
            let raw_line = line.clone();
            if matches!(line.last(), Some(b'\n')) {
                line.pop();
            }
            if matches!(line.last(), Some(b'\r')) {
                line.pop();
            }

            // Skip empty/whitespace-only lines (compat).
            let trimmed = line
                .iter()
                .skip_while(|b| is_ascii_whitespace(**b))
                .copied()
                .collect::<Vec<u8>>();
            if trimmed.is_empty() {
                continue;
            }

            // If we see a Content-Length header while in newline mode, switch modes and requeue.
            if starts_with_content_length(&trimmed) {
                // Preserve the original line terminator width (`\n` vs `\r\n`): mixing newline
                // styles can prevent the Content-Length parser from finding the header/body
                // boundary (`\r\n\r\n` or `\n\n`).
                let mut rebuilt = raw_line;
                rebuilt.extend_from_slice(&self.buf);
                self.buf = rebuilt;
                self.framing = Framing::ContentLength;
                return self.try_decode();
            }

            match serde_json::from_slice::<RxJsonRpcMessage<Role>>(&trimmed) {
                Ok(msg) => return Ok(Some(msg)),
                Err(err) => {
                    if matches!(trimmed.first(), Some(b'[')) {
                        match serde_json::from_slice::<Vec<RxJsonRpcMessage<Role>>>(&trimmed) {
                            Ok(batch) => return Ok(self.queue_batch(batch)),
                            Err(batch_err) => {
                                return Err(io::Error::new(io::ErrorKind::InvalidData, batch_err))
                            }
                        }
                    }

                    // Compat: ignore non-JSON garbage lines (but keep strict for JSON-looking lines).
                    if matches!(trimmed.first(), Some(b'{')) {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, err));
                    }
                }
            }
        }
    }

    fn try_decode_content_length(&mut self) -> Result<Option<RxJsonRpcMessage<Role>>, io::Error> {
        let Some((header_end, _width)) = find_double_newline(&self.buf) else {
            return Ok(None);
        };
        let header_bytes = self.buf[..header_end].to_vec();
        let header_str = std::str::from_utf8(&header_bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let Some(len) = parse_content_length(header_str) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing Content-Length header",
            ));
        };

        if len > MAX_MESSAGE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Content-Length {len} exceeds maximum supported message size {MAX_MESSAGE_BYTES}"
                ),
            ));
        }
        if header_end + len > MAX_BUFFER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "message size {} exceeds maximum buffer size {MAX_BUFFER_BYTES}",
                    header_end + len
                ),
            ));
        }

        if self.buf.len() < header_end + len {
            return Ok(None);
        }

        let body = self.buf[header_end..header_end + len].to_vec();
        self.buf.drain(..header_end + len);

        match serde_json::from_slice::<RxJsonRpcMessage<Role>>(&body) {
            Ok(msg) => Ok(Some(msg)),
            Err(err) => {
                if matches!(body.first(), Some(b'[')) {
                    let batch = serde_json::from_slice::<Vec<RxJsonRpcMessage<Role>>>(&body)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    Ok(self.queue_batch(batch))
                } else {
                    Err(io::Error::new(io::ErrorKind::InvalidData, err))
                }
            }
        }
    }

    fn try_decode(&mut self) -> Result<Option<RxJsonRpcMessage<Role>>, io::Error> {
        self.detect_framing();

        match self.framing {
            Framing::Unknown => Ok(None),
            Framing::NewlineJson => self.try_decode_newline(),
            Framing::ContentLength => self.try_decode_content_length(),
        }
    }
}

impl<Role: ServiceRole, R, W> Transport<Role> for HybridStdioTransport<Role, R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
{
    type Error = io::Error;

    fn name() -> Cow<'static, str> {
        "HybridStdioTransport".into()
    }

    fn send(
        &mut self,
        item: TxJsonRpcMessage<Role>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let framing = self.framing;
        let write_tx = self.write_tx.clone();

        async move {
            let Some(write_tx) = write_tx else {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "transport closed",
                ));
            };
            let mut value = serde_json::to_value(&item).map_err(io::Error::other)?;
            sanitize_tools_list_schema(&mut value);
            let json = serde_json::to_vec(&value).map_err(io::Error::other)?;

            let mut out = Vec::new();
            match framing {
                Framing::ContentLength => {
                    out.extend_from_slice(
                        format!("Content-Length: {}\r\n\r\n", json.len()).as_bytes(),
                    );
                    out.extend_from_slice(&json);
                }
                Framing::Unknown | Framing::NewlineJson => {
                    out.extend_from_slice(&json);
                    out.push(b'\n');
                }
            }

            let (reply_tx, reply_rx) = oneshot::channel::<io::Result<()>>();
            write_tx
                .send(WriteRequest {
                    bytes: out,
                    reply: reply_tx,
                })
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::NotConnected, "transport closed"))?;
            reply_rx
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::NotConnected, "transport closed"))??;
            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<Role>> {
        loop {
            if let Some(msg) = self.pending.pop_front() {
                return Some(msg);
            }

            match self.try_decode() {
                Ok(Some(msg)) => return Some(msg),
                Ok(None) => {}
                Err(err) => {
                    // Mirror rmcp behavior: log and terminate the stream.
                    log::error!("Error reading from stream: {err}");
                    return None;
                }
            }

            let mut tmp = [0u8; 8192];
            let n = match self.read.read(&mut tmp).await {
                Ok(n) => n,
                Err(err) => {
                    log::error!("Error reading from stream: {err}");
                    return None;
                }
            };
            if n == 0 {
                // EOF: some tool runners write a single request then close stdin. Before treating
                // the stream as closed, attempt one last decode from any buffered bytes.
                match self.try_decode() {
                    Ok(Some(msg)) => return Some(msg),
                    Ok(None) => return None,
                    Err(err) => {
                        log::error!("Error decoding buffered message at EOF: {err}");
                        return None;
                    }
                }
            }
            if let Some(dump) = self.dump.as_ref() {
                if let Ok(mut guard) = dump.lock() {
                    let line = FrameDumpLine {
                        ts_ms: now_unix_ms(),
                        dir: "rx",
                        len: n,
                        b64: base64::engine::general_purpose::STANDARD.encode(&tmp[..n]),
                    };
                    if let Ok(payload) = serde_json::to_string(&line) {
                        let _ = writeln!(guard.file, "{payload}");
                    }
                }
            }
            self.buf.extend_from_slice(&tmp[..n]);
            if self.buf.len() > MAX_BUFFER_BYTES {
                log::error!(
                    "Input buffer exceeded maximum size ({} > {MAX_BUFFER_BYTES}); closing transport",
                    self.buf.len()
                );
                return None;
            }
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let write_task = self.write_task.take();
        self.write_tx.take();
        async move {
            if let Some(task) = write_task {
                task.abort();
                let _ = task.await;
            }
            Ok(())
        }
    }
}

pub fn stdio_hybrid_server(
) -> HybridStdioTransport<rmcp::RoleServer, tokio::io::Stdin, tokio::io::Stdout> {
    HybridStdioTransport::new(tokio::io::stdin(), tokio::io::stdout())
}

/// Transport wrapper that makes rmcp server sessions tolerant of clients that skip or reorder
/// `notifications/initialized`.
///
/// Some MCP clients send `initialize`, then immediately `tools/list` / `tools/call` without sending
/// `notifications/initialized`. rmcp expects the notification and will terminate the session.
///
/// We treat "first request after initialize" as implicit initialized, synthesize the missing
/// notification once, and drop duplicate notifications that arrive later.
pub struct InitializedCompatTransport<R, T>
where
    R: ServiceRole,
    T: Transport<R>,
{
    // Note: shared backend proxy already handles "no initialize" clients for the daemon session,
    // but we still want in-process mode to be agent-native as well.
    inner: T,
    initialize_seen: bool,
    initialized_forwarded: bool,
    synthesized_initialize: bool,
    synth_init_pending: bool,
    synth_initialized_pending: bool,
    first_message: Option<RxJsonRpcMessage<R>>,
    pending: Option<RxJsonRpcMessage<R>>,
    _marker: PhantomData<R>,
}

impl<R, T> InitializedCompatTransport<R, T>
where
    R: ServiceRole,
    T: Transport<R>,
{
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            initialize_seen: false,
            initialized_forwarded: false,
            synthesized_initialize: false,
            synth_init_pending: false,
            synth_initialized_pending: false,
            first_message: None,
            pending: None,
            _marker: PhantomData,
        }
    }
}

impl<R, T> Transport<R> for InitializedCompatTransport<R, T>
where
    R: ServiceRole,
    T: Transport<R>,
{
    type Error = T::Error;

    fn name() -> Cow<'static, str> {
        "InitializedCompatTransport".into()
    }

    fn send(
        &mut self,
        item: TxJsonRpcMessage<R>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        const SYNTH_INIT_ID: &str = "__cf_synth_init__";
        if self.synthesized_initialize {
            if let Ok(value) = serde_json::to_value(&item) {
                if value.get("id").and_then(Value::as_str) == Some(SYNTH_INIT_ID) {
                    // Handshake-less clients didn't ask for initialize; swallow the synthetic
                    // initialize response so only tool results hit their transport.
                    return Box::pin(async { Ok(()) })
                        as std::pin::Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>>;
                }
            }
        }

        let fut = self.inner.send(item);
        Box::pin(fut) as std::pin::Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>>
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<R>> {
        const SYNTH_INIT_ID: &str = "__cf_synth_init__";
        const SYNTH_PROTOCOL_VERSION: &str = "2024-11-05";
        loop {
            if self.synth_init_pending {
                self.synth_init_pending = false;
                self.synth_initialized_pending = true;
                self.initialize_seen = true;
                let init_req = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": SYNTH_INIT_ID,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": SYNTH_PROTOCOL_VERSION,
                        "capabilities": {},
                        "clientInfo": { "name": "context-compat", "version": env!("CARGO_PKG_VERSION") }
                    }
                });
                if let Ok(tx) = serde_json::from_value::<RxJsonRpcMessage<R>>(init_req) {
                    return Some(tx);
                }
            }
            if self.synth_initialized_pending {
                self.synth_initialized_pending = false;
                self.initialized_forwarded = true;
                let init_not = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                });
                if let Ok(tx) = serde_json::from_value::<RxJsonRpcMessage<R>>(init_not) {
                    return Some(tx);
                }
            }
            if let Some(first) = self.first_message.take() {
                return Some(first);
            }
            if let Some(pending) = self.pending.take() {
                return Some(pending);
            }

            let msg = self.inner.receive().await?;
            let value: Value = match serde_json::to_value(&msg) {
                Ok(value) => value,
                Err(_) => return Some(msg),
            };
            let method = value.get("method").and_then(Value::as_str);

            if self.synthesized_initialize && method == Some("initialize") {
                // If the client starts sending real initialize messages after we already
                // synthesized one, ignore to keep the session stable.
                continue;
            }

            if method == Some("initialize") {
                self.initialize_seen = true;
                return Some(msg);
            }

            if method == Some("notifications/initialized") {
                if !self.initialize_seen {
                    // Ignore out-of-order initialized; we'll synthesize a full handshake once we
                    // see the first real request.
                    continue;
                }
                if self.initialized_forwarded {
                    // Drop duplicates (we may have synthesized one already).
                    continue;
                }
                self.initialized_forwarded = true;
                return Some(msg);
            }

            if self.initialize_seen && !self.initialized_forwarded {
                self.pending = Some(msg);
                self.initialized_forwarded = true;
                let init_not = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                });
                if let Ok(tx) = serde_json::from_value::<RxJsonRpcMessage<R>>(init_not) {
                    return Some(tx);
                }
                return self.pending.take();
            }

            if !self.initialize_seen {
                // Agent-native robustness: accept tool calls without MCP handshake by synthesizing
                // `initialize` + `notifications/initialized` for rmcp, and swallowing the init
                // response.
                self.synthesized_initialize = true;
                self.first_message = Some(msg);
                self.synth_init_pending = true;
                continue;
            }

            return Some(msg);
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}

pub fn stdio_hybrid_server_agent() -> InitializedCompatTransport<
    rmcp::RoleServer,
    HybridStdioTransport<rmcp::RoleServer, tokio::io::Stdin, tokio::io::Stdout>,
> {
    InitializedCompatTransport::new(stdio_hybrid_server())
}

/// Transport wrapper that makes one-shot tool runners robust:
/// if the client closes stdin after sending requests, keep the session alive
/// until all pending request ids have been responded to (or a bounded timeout).
pub struct EofDrainTransport<R, T>
where
    R: ServiceRole,
    T: Transport<R>,
{
    inner: T,
    pending_request_ids: Arc<Mutex<HashSet<RequestId>>>,
    pending_notify: Arc<tokio::sync::Notify>,
    input_closed: bool,
    drain_started_at: Option<Instant>,
    drain_timeout: Duration,
    _marker: PhantomData<R>,
}

impl<R, T> EofDrainTransport<R, T>
where
    R: ServiceRole,
    T: Transport<R>,
{
    const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(120);
    const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(5);

    pub fn new(inner: T) -> Self {
        Self {
            inner,
            pending_request_ids: Arc::new(Mutex::new(HashSet::new())),
            pending_notify: Arc::new(tokio::sync::Notify::new()),
            input_closed: false,
            drain_started_at: None,
            drain_timeout: Self::DEFAULT_DRAIN_TIMEOUT,
            _marker: PhantomData,
        }
    }

    fn track_incoming(&mut self, msg: &RxJsonRpcMessage<R>) {
        if let JsonRpcMessage::Request(req) = msg {
            if let Ok(mut guard) = self.pending_request_ids.lock() {
                guard.insert(req.id.clone());
            }
        }
    }

    fn outgoing_response_id(msg: &TxJsonRpcMessage<R>) -> Option<RequestId> {
        match msg {
            JsonRpcMessage::Response(resp) => Some(resp.id.clone()),
            JsonRpcMessage::Error(err) => Some(err.id.clone()),
            _ => None,
        }
    }
}

impl<R, T> Transport<R> for EofDrainTransport<R, T>
where
    R: ServiceRole,
    T: Transport<R> + Send + 'static,
{
    type Error = T::Error;

    fn name() -> Cow<'static, str> {
        "EofDrainTransport".into()
    }

    fn send(
        &mut self,
        item: TxJsonRpcMessage<R>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let response_id = Self::outgoing_response_id(&item);
        let pending = self.pending_request_ids.clone();
        let notify = self.pending_notify.clone();
        let send = self.inner.send(item);

        async move {
            let result = send.await;
            if let Some(id) = response_id {
                if let Ok(mut guard) = pending.lock() {
                    guard.remove(&id);
                }
                notify.notify_waiters();
            }
            result
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<R>> {
        loop {
            if self.input_closed {
                let pending_empty = self
                    .pending_request_ids
                    .lock()
                    .is_ok_and(|guard| guard.is_empty());
                if pending_empty {
                    return None;
                }
                if let Some(started_at) = self.drain_started_at {
                    if started_at.elapsed() > self.drain_timeout {
                        return None;
                    }
                }
                let notified = self.pending_notify.notified();
                let _ = tokio::time::timeout(Self::DRAIN_POLL_INTERVAL, notified).await;
                continue;
            }

            let msg = self.inner.receive().await;
            match msg {
                Some(msg) => {
                    self.track_incoming(&msg);
                    return Some(msg);
                }
                None => {
                    self.input_closed = true;
                    self.drain_started_at = Some(Instant::now());
                    // Loop: we'll wait until pending requests are flushed.
                    continue;
                }
            }
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}

pub fn stdio_hybrid_server_agent_oneshot_safe() -> EofDrainTransport<
    rmcp::RoleServer,
    InitializedCompatTransport<
        rmcp::RoleServer,
        HybridStdioTransport<rmcp::RoleServer, tokio::io::Stdin, tokio::io::Stdout>,
    >,
> {
    EofDrainTransport::new(stdio_hybrid_server_agent())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, DuplexStream};

    fn split_duplex(
        stream: DuplexStream,
    ) -> (
        tokio::io::ReadHalf<DuplexStream>,
        tokio::io::WriteHalf<DuplexStream>,
    ) {
        tokio::io::split(stream)
    }

    #[tokio::test]
    async fn rejects_excessive_content_length() {
        let (mut client, server) = tokio::io::duplex(16_384);
        let (read, write) = split_duplex(server);
        let mut transport = HybridStdioTransport::<rmcp::RoleServer, _, _>::new(read, write);

        client
            .write_all(b"Content-Length: 999999\r\n\r\n")
            .await
            .expect("write header");
        client.flush().await.expect("flush");
        drop(client);

        let msg = transport.receive().await;
        assert!(msg.is_none());
    }

    #[tokio::test]
    async fn closes_on_newline_mode_buffer_overflow() {
        let (mut client, server) = tokio::io::duplex(16_384);
        let (read, write) = split_duplex(server);
        let mut transport = HybridStdioTransport::<rmcp::RoleServer, _, _>::new(read, write);

        let payload = vec![b'a'; MAX_BUFFER_BYTES + 1];
        client.write_all(&payload).await.expect("write payload");
        client.flush().await.expect("flush");
        drop(client);

        let msg = transport.receive().await;
        assert!(msg.is_none());
    }
}
