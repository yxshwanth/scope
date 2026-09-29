use code_scope::{Symbol, SymbolKind, index};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

mod buffers;

type Pending = HashMap<String, (Value, u64, Arc<AtomicBool>)>;

struct Job {
    buffers: buffers::OpenBuffers,
    key: String,
    ticket: u64,
    query: String,
    cancelled: Arc<AtomicBool>,
}

enum Event {
    Input(Value),
    ParseError,
    Closed(Option<String>),
    Done(String, u64, Result<Vec<Value>, String>),
}

// Only the main thread writes stdout. Indexing never blocks protocol dispatch.
pub fn run(root: &Path) -> io::Result<bool> {
    let root = root.canonicalize()?;
    if !root.is_dir() {
        return Err(io::Error::other("LSP root must be a directory"));
    }

    let (events, incoming) = mpsc::sync_channel(64);
    let reader_events = events.clone();

    thread::Builder::new()
        .name("scope-lsp-input".into())
        .spawn(move || {
            let mut input = BufReader::new(io::stdin());
            loop {
                let event = match read_packet(&mut input) {
                    Ok(Some(body)) => match serde_json::from_slice(&body) {
                        Ok(value) => Event::Input(value),
                        Err(_) => Event::ParseError,
                    },
                    Ok(None) => Event::Closed(None),
                    Err(error) => Event::Closed(Some(error.to_string())),
                };
                let closed = matches!(event, Event::Closed(_));
                if reader_events.send(event).is_err() || closed {
                    break;
                }
            }
        })?;

    let workspace_root = root.clone();
    let (jobs, work) = mpsc::sync_channel::<Job>(8);

    thread::Builder::new()
        .name("scope-lsp-index".into())
        .spawn(move || {
            for job in work {
                if job.cancelled.load(Ordering::Relaxed) {
                    continue;
                }
                let result = std::panic::catch_unwind(|| {
                    search(&root, &job.query, &job.cancelled, &job.buffers)
                })
                .unwrap_or_else(|_| Err("index worker panicked".into()));

                if events
                    .send(Event::Done(job.key, job.ticket, result))
                    .is_err()
                {
                    break;
                }
            }
        })?;

    let mut output = io::stdout().lock();
    let mut pending = Pending::new();
    let mut open_buffers = buffers::OpenBuffers::new();
    let mut ticket = 0;
    let mut initialized = false;
    let mut shutdown = false;

    // Do not join stdin or the worker here: stdin can remain open after exit,
    // and the worker may be waiting on SQLite. Returning from main ends them.
    while let Ok(event) = incoming.recv() {
        let message = match event {
            Event::ParseError => {
                reply_error(&mut output, Value::Null, -32700, "invalid JSON")?;
                continue;
            }
            Event::Closed(error) => {
                return match error {
                    Some(error) => Err(io::Error::other(error)),
                    None => Ok(shutdown),
                };
            }
            Event::Done(key, generation, result) => {
                if pending.get(&key).is_some_and(|(_, t, _)| *t == generation) {
                    let (id, _, _) = pending.remove(&key).unwrap();
                    match result {
                        Ok(symbols) => send(
                            &mut output,
                            json!({
                                "jsonrpc":"2.0", "id":id, "result":symbols
                            }),
                        )?,
                        Err(error) => reply_error(&mut output, id, -32603, &error)?,
                    }
                }
                continue;
            }
            Event::Input(message) => message,
        };

        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str);

        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || method.is_none()
            || id
                .as_ref()
                .is_some_and(|id| !id.is_string() && !id.is_i64() && !id.is_u64())
        {
            reply_error(&mut output, Value::Null, -32600, "invalid request")?;
            continue;
        }

        let method = method.unwrap();

        if id.is_none() {
            if initialized && !shutdown {
                match buffers::update(&workspace_root, &mut open_buffers, &message) {
                    Ok(true) => {
                        for (_, (id, _, flag)) in pending.drain() {
                            flag.store(true, Ordering::Relaxed);
                            reply_error(
                                &mut output,
                                id,
                                -32801,
                                "Buffer changed; run the search again",
                            )?;
                        }
                    }
                    Ok(false) => {}
                    Err(error) => send(
                        &mut output,
                        json!({
                            "jsonrpc":"2.0", "method":"window/logMessage",
                            "params":{"type":2,"message":error}
                        }),
                    )?,
                }
            }
            if method == "exit" {
                return Ok(shutdown);
            }
            if method == "$/cancelRequest"
                && let Some(id) = message.pointer("/params/id")
                && let Some((id, _, flag)) = pending.remove(&id.to_string())
            {
                flag.store(true, Ordering::Relaxed);
                reply_error(&mut output, id, -32800, "request cancelled")?;
            }
            continue;
        }

        let id = id.unwrap();
        let key = id.to_string();

        if pending.contains_key(&key) {
            return Err(io::Error::other("duplicate outstanding request id"));
        }

        if shutdown {
            reply_error(&mut output, id, -32600, "server has shut down")?;
        } else if method == "initialize" && !initialized {
            if !message.get("params").is_some_and(Value::is_object) {
                reply_error(
                    &mut output,
                    id,
                    -32602,
                    "initialize params must be an object",
                )?;
                continue;
            }
            initialized = true;
            send(
                &mut output,
                json!({
                    "jsonrpc":"2.0",
                    "id":id,
                    "result":{
                        "capabilities":{
                            "workspaceSymbolProvider":true,
                            "positionEncoding":"utf-16",
                            "textDocumentSync":{"openClose":true,"change":1}
                        },
                        "serverInfo":{
                            "name":"scope",
                            "version":env!("CARGO_PKG_VERSION")
                        }
                    }
                }),
            )?;
        } else if !initialized {
            reply_error(&mut output, id, -32002, "server not initialized")?;
        } else if method == "shutdown" {
            shutdown = true;
            for (_, (id, _, flag)) in pending.drain() {
                flag.store(true, Ordering::Relaxed);
                reply_error(&mut output, id, -32800, "server shutting down")?;
            }
            send(
                &mut output,
                json!({
                    "jsonrpc":"2.0", "id":id, "result":null
                }),
            )?;
        } else if method == "workspace/symbol" {
            let Some(query) = message.pointer("/params/query").and_then(Value::as_str) else {
                reply_error(&mut output, id, -32602, "query must be a string")?;
                continue;
            };

            if let Err(error) = code_scope::search::Query::parse(query) {
                reply_error(&mut output, id, -32602, &error)?;
                continue;
            }
            ticket += 1;
            let cancelled = Arc::new(AtomicBool::new(false));
            let job = Job {
                buffers: open_buffers.clone(),
                key: key.clone(),
                ticket,
                query: query.into(),
                cancelled: cancelled.clone(),
            };

            match jobs.try_send(job) {
                Ok(()) => {
                    pending.insert(key, (id, ticket, cancelled));
                }
                Err(_) => reply_error(&mut output, id, -32000, "index queue busy; retry search")?,
            }
        } else {
            reply_error(&mut output, id, -32601, "method not supported")?;
        }
    }

    Ok(shutdown)
}

fn send(output: &mut impl Write, value: Value) -> io::Result<()> {
    let body = serde_json::to_vec(&value).map_err(io::Error::other)?;
    write!(output, "Content-Length: {}\r\n\r\n", body.len())?;
    output.write_all(&body)?;
    output.flush()
}

fn reply_error(output: &mut impl Write, id: Value, code: i32, message: &str) -> io::Result<()> {
    send(
        output,
        json!({
            "jsonrpc":"2.0", "id":id,
            "error":{"code":code,"message":message}
        }),
    )
}

fn read_packet(input: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut header = Vec::new();
    let mut byte = [0];

    while !header.ends_with(b"\r\n\r\n") {
        if input.read(&mut byte)? == 0 {
            return if header.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::other("truncated LSP header"))
            };
        }
        header.push(byte[0]);
        if header.len() > 8192 {
            return Err(io::Error::other("LSP header too large"));
        }
    }

    let header = std::str::from_utf8(&header).map_err(io::Error::other)?;
    let mut length = None;

    for line in header.split("\r\n").filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::other("invalid LSP header"))?;
        if name.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                return Err(io::Error::other("duplicate Content-Length"));
            }
            length = Some(value.trim().parse::<usize>().map_err(io::Error::other)?);
        }
    }

    let length = length
        .filter(|n| *n <= 8 * 1024 * 1024)
        .ok_or_else(|| io::Error::other("missing or oversized Content-Length"))?;
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    Ok(Some(body))
}

fn search(
    root: &Path,
    query: &str,
    cancelled: &AtomicBool,
    open: &buffers::OpenBuffers,
) -> Result<Vec<Value>, String> {
    let query = code_scope::search::Query::parse(query)?;
    index::rebuild(root).map_err(|e| e.to_string())?;
    if cancelled.load(Ordering::Relaxed) {
        return Ok(Vec::new());
    }

    let mut symbols = index::find(root, "").map_err(|e| e.to_string())?;
    symbols.retain(|symbol| !open.contains_key(&symbol.path));
    let mut files: HashMap<PathBuf, Vec<String>> = HashMap::new();

    for (path, buffer) in open {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(Vec::new());
        }
        symbols.extend(
            code_scope::extract_symbols(path, buffer.source.as_bytes(), buffer.language)
                .map_err(|e| e.to_string())?,
        );
        files.insert(
            path.clone(),
            buffer.source.lines().map(str::to_owned).collect(),
        );
    }

    let symbols = query.rank(symbols, root);
    let mut result = Vec::with_capacity(symbols.len());

    for symbol in symbols {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(Vec::new());
        }
        if let std::collections::hash_map::Entry::Vacant(entry) = files.entry(symbol.path.clone()) {
            let source = fs::read_to_string(&symbol.path).map_err(|e| e.to_string())?;
            entry.insert(source.lines().map(str::to_owned).collect());
        }
        let row = symbol.line.checked_sub(1).ok_or("invalid symbol line")?;
        let line = files[&symbol.path]
            .get(row)
            .ok_or("file changed during search; retry")?;
        result.push(symbol_info(&symbol, line)?);
    }
    Ok(result)
}

fn symbol_info(symbol: &Symbol, line: &str) -> Result<Value, String> {
    let column = symbol
        .column
        .checked_sub(1)
        .ok_or("invalid symbol column")?;
    let prefix = line
        .get(..column)
        .ok_or("file changed during search; retry")?;

    if !line[column..].starts_with(&symbol.name) {
        return Err("file changed during search; retry".into());
    }

    let start = prefix.encode_utf16().count();
    let end = start + symbol.name.encode_utf16().count();
    let row = symbol.line.checked_sub(1).ok_or("invalid symbol line")?;
    let kind = match symbol.kind {
        SymbolKind::Function => 12,
        SymbolKind::Method => 6,
        SymbolKind::Type => 5,
        SymbolKind::Trait => 11,
    };

    Ok(json!({
        "name":symbol.name, "containerName":symbol.container,
        "kind":kind,
        "location":{
            "uri":file_uri(&symbol.path)?,
            "range":{
                "start":{"line":row,"character":start},
                "end":{"line":row,"character":end}
            }
        }
    }))
}

fn file_uri(path: &Path) -> Result<String, String> {
    let path = path.to_str().ok_or("non-UTF-8 path")?;
    if !path.starts_with('/') {
        return Err("this LSP slice requires a Unix absolute path".into());
    }

    let mut uri = String::from("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_positions_and_escaped_uri() {
        let line = "/* 🦀é */ fn café() {}";
        let symbol = Symbol {
            container: None,
            name: "café".into(),
            kind: SymbolKind::Function,
            path: "/tmp/a #é.rs".into(),
            line: 1,
            column: line.find("café").unwrap() + 1,
        };
        let value = symbol_info(&symbol, line).unwrap();
        assert_eq!(value["location"]["range"]["start"]["character"], 13);
        assert_eq!(value["location"]["range"]["end"]["character"], 17);
        assert_eq!(value["location"]["uri"], "file:///tmp/a%20%23%C3%A9.rs");
    }

    #[test]
    fn framing_uses_bytes_and_preserves_next_packet() {
        let mut bytes = Vec::new();
        send(&mut bytes, json!({"name":"🦀"})).unwrap();
        send(&mut bytes, json!({"id":2})).unwrap();

        let mut input = io::Cursor::new(bytes);
        let first: Value =
            serde_json::from_slice(&read_packet(&mut input).unwrap().unwrap()).unwrap();

        assert_eq!(first["name"], "🦀");
        assert!(read_packet(&mut input).unwrap().is_some());
        assert!(read_packet(&mut input).unwrap().is_none());
        assert!(read_packet(&mut &b"Content-Length: 9000000\r\n\r\n"[..]).is_err());
        assert!(read_packet(&mut &b"Content-Length: 4\r\n\r\nx"[..]).is_err());
    }
}
