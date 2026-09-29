use code_scope::Language;
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

pub(super) struct Buffer {
    pub source: String,
    pub language: Language,
    version: i64,
}
pub(super) type OpenBuffers = HashMap<PathBuf, Arc<Buffer>>;

// Return true only when the searchable buffer state actually changes.
pub(super) fn update(root: &Path, open: &mut OpenBuffers, message: &Value) -> Result<bool, String> {
    let method = message["method"].as_str().unwrap_or("");
    if !matches!(
        method,
        "textDocument/didOpen" | "textDocument/didChange" | "textDocument/didClose"
    ) {
        return Ok(false);
    }
    let document = &message["params"]["textDocument"];
    let uri = document["uri"].as_str().ok_or("missing document URI")?;
    if !uri.starts_with("file://") {
        return Ok(false);
    }
    let path = path_from_uri(uri)?;
    if !path.starts_with(root) {
        return Ok(false);
    }
    let Some(language) = Language::from_path(&path) else {
        return Ok(false);
    };
    if method == "textDocument/didClose" {
        return Ok(open.remove(&path).is_some());
    }
    let version = document["version"]
        .as_i64()
        .ok_or("missing document version")?;
    if open.get(&path).is_some_and(|old| version <= old.version) {
        return Ok(false);
    }
    let source = if method == "textDocument/didOpen" {
        document["text"].as_str().ok_or("missing document text")?
    } else {
        if !open.contains_key(&path) {
            return Err("change received before didOpen".into());
        }
        let changes = message["params"]["contentChanges"]
            .as_array()
            .ok_or("missing contentChanges")?;
        if changes.is_empty() {
            return Ok(false);
        }
        for change in changes {
            if change.get("range").is_some() || change["text"].as_str().is_none() {
                return Err("Scope requires full-document text changes".into());
            }
        }
        changes.last().unwrap()["text"].as_str().unwrap()
    };
    open.insert(
        path,
        Arc::new(Buffer {
            source: source.into(),
            language,
            version,
        }),
    );
    Ok(true)
}

fn path_from_uri(uri: &str) -> Result<PathBuf, String> {
    let rest = uri.strip_prefix("file://").ok_or("expected file URI")?;
    let encoded = rest.strip_prefix("localhost").unwrap_or(rest);
    if !encoded.starts_with('/') || encoded.contains('?') || encoded.contains('#') {
        return Err("expected a local absolute file URI".into());
    }
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let pair = bytes.get(i + 1..i + 3).ok_or("incomplete URI escape")?;
            let high = (pair[0] as char).to_digit(16).ok_or("invalid URI escape")?;
            let low = (pair[1] as char).to_digit(16).ok_or("invalid URI escape")?;
            decoded.push((high * 16 + low) as u8);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    let text = String::from_utf8(decoded).map_err(|e| e.to_string())?;
    if text.contains('\0') {
        return Err("NUL in file URI".into());
    }
    let mut path = PathBuf::from("/");
    for component in Path::new(&text).components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::ParentDir => {
                path.pop();
            }
            _ => {}
        }
    }
    // Resolve directory aliases, while preserving the filename as the index does.
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(parent) = parent.canonicalize()
    {
        return Ok(parent.join(name));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn versions_close_and_workspace_boundary() {
        let root = Path::new("/scope-buffer-fixture");
        let mut open = OpenBuffers::new();
        let uri = "file:///scope-buffer-fixture/a%20%23%C3%A9.rs";
        let opened = json!({"method":"textDocument/didOpen","params":{"textDocument":{
            "uri":uri,"version":1,"text":"fn saved() {}"
        }}});
        assert!(update(root, &mut open, &opened).unwrap());
        let change = |version, text| {
            json!({"method":"textDocument/didChange","params":{
                "textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":text}]
            }})
        };
        assert!(update(root, &mut open, &change(3, "fn draft() {}")).unwrap());
        assert!(!update(root, &mut open, &change(2, "fn stale() {}")).unwrap());
        assert_eq!(open.values().next().unwrap().source, "fn draft() {}");
        assert!(open.contains_key(&root.join("a #é.rs")));
        assert!(
            update(
                root,
                &mut open,
                &json!({"method":"textDocument/didClose","params":{
                    "textDocument":{"uri":uri}
                }})
            )
            .unwrap()
        );
        assert!(open.is_empty());
        let mut outside = opened;
        outside["params"]["textDocument"]["uri"] =
            json!("file:///scope-buffer-fixture/../elsewhere/a.rs");
        assert!(!update(root, &mut open, &outside).unwrap());
    }

    #[test]
    fn rejects_bad_uris() {
        for uri in [
            "file://remote/a.rs",
            "file:///a%GG.rs",
            "file:///a%",
            "file:///a%00.rs",
        ] {
            assert!(path_from_uri(uri).is_err(), "{uri}");
        }
    }
}
