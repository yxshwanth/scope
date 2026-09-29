use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};

use crate::{
    MAX_SOURCE_BYTES, ScanError, ScanWarning, Symbol, SymbolKind, discover_sources, extract_symbols,
};

const INDEX_FILE: &str = "index.sqlite";

#[derive(Debug)]
pub struct IndexReport {
    pub files_parsed: usize,
    pub files_unchanged: usize,
    pub files_removed: usize,
    pub symbols_indexed: usize,
    pub database_path: PathBuf,
}

#[derive(Debug)]
pub enum IndexError {
    Query(String),
    Io(std::io::Error),
    Database(rusqlite::Error),
    Scan(ScanError),
    IncompleteScan(usize, ScanWarning),
    SourceFile(PathBuf, String),
    NotDirectory(PathBuf),
    MissingIndex(PathBuf),
    NonUtf8Path(PathBuf),
    MissingHome,
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Query(message) => write!(f, "invalid query: {message}"),
            Self::Io(error) => write!(f, "filesystem error: {error}"),
            Self::Database(error) => write!(f, "index database error: {error}"),
            Self::Scan(error) => write!(f, "scan failed: {error}"),
            Self::IncompleteScan(count, warning) => write!(
                f,
                "index unchanged: {count} scan warning(s); first at {}: {}",
                warning.path.display(),
                warning.message
            ),
            Self::SourceFile(path, message) => {
                write!(f, "index unchanged: {}: {message}", path.display())
            }
            Self::NotDirectory(path) => {
                write!(f, "index root must be a directory: {}", path.display())
            }
            Self::MissingIndex(path) => write!(
                f,
                "no index at {}; run `scope index PATH` first",
                path.display()
            ),
            Self::NonUtf8Path(path) => {
                write!(f, "cannot index a non-UTF-8 path: {}", path.display())
            }
            Self::MissingHome => f.write_str("cannot locate your home directory for Scope's cache"),
        }
    }
}

impl Error for IndexError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Database(error) => Some(error),
            Self::Scan(error) => Some(error),
            _ => None,
        }
    }
}

struct ChangedFile {
    path: String,
    hash: String,
    symbols: Vec<Symbol>,
}

/// Reparse changed files and atomically update the saved index.
/// A failed scan keeps the previous symbols available to readers.
pub fn rebuild(root: &Path) -> Result<IndexReport, IndexError> {
    let root = canonical_directory(root)?;
    let database_path = database_path(&root)?;
    fs::create_dir_all(
        database_path
            .parent()
            .expect("index path has a parent directory"),
    )
    .map_err(IndexError::Io)?;

    let mut connection = Connection::open(&database_path).map_err(IndexError::Database)?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS symbols (
                path TEXT NOT NULL,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                line INTEGER NOT NULL,
                column INTEGER NOT NULL,
                container TEXT
            );
            CREATE INDEX IF NOT EXISTS symbols_name ON symbols(name);
            CREATE TABLE IF NOT EXISTS files (
                path TEXT PRIMARY KEY,
                hash TEXT NOT NULL
            );",
        )
        .map_err(IndexError::Database)?;

    // Serialize writers before reading hashes so concurrent indexers cannot
    // apply changes based on an outdated database snapshot. Readers can continue.
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(IndexError::Database)?;
    if !has_container(&transaction)? {
        transaction
            .execute_batch("ALTER TABLE symbols ADD COLUMN container TEXT; DELETE FROM files;")
            .map_err(IndexError::Database)?;
    }
    let previous: HashMap<String, String> = {
        let mut statement = transaction
            .prepare("SELECT path, hash FROM files")
            .map_err(IndexError::Database)?;
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(IndexError::Database)?;
        rows.collect::<rusqlite::Result<_>>()
            .map_err(IndexError::Database)?
    };

    let discovery = discover_sources(&root).map_err(IndexError::Scan)?;
    if !discovery.warnings.is_empty() {
        let count = discovery.warnings.len();
        let first = discovery.warnings.into_iter().next().unwrap();
        return Err(IndexError::IncompleteScan(count, first));
    }

    // Workers borrow only immutable data. Each task owns its source bytes and
    // parser; SQLite stays here. Collect succeeds before symbol/hash writes.
    let prepared = discovery
        .paths
        .into_par_iter()
        .map(|(path, language)| {
            let relative = path
                .strip_prefix(&root)
                .expect("discovered path is under root");
            let relative = relative
                .to_str()
                .ok_or_else(|| IndexError::NonUtf8Path(path.clone()))?
                .to_owned();
            let source = fs::read(&path)
                .map_err(|error| IndexError::SourceFile(path.clone(), error.to_string()))?;
            if source.len() as u64 > MAX_SOURCE_BYTES {
                return Err(IndexError::SourceFile(
                    path,
                    "source file exceeds the 2 MiB first-slice limit".to_owned(),
                ));
            }
            let hash = blake3::hash(&source).to_hex().to_string();
            if previous.get(&relative) == Some(&hash) {
                return Ok((relative, None));
            }
            let symbols = extract_symbols(&path, &source, language)
                .map_err(|error| IndexError::SourceFile(path, error.to_string()))?;
            let file = ChangedFile {
                path: relative.clone(),
                hash,
                symbols,
            };
            Ok((relative, Some(file)))
        })
        .collect::<Result<Vec<_>, IndexError>>()?;

    let mut seen = HashSet::with_capacity(prepared.len());
    let mut changed = Vec::new();
    for (path, file) in prepared {
        seen.insert(path);
        if let Some(file) = file {
            changed.push(file);
        }
    }
    let removed: Vec<_> = previous
        .keys()
        .filter(|path| !seen.contains(*path))
        .collect();

    // Existing v0.2 caches have symbols but no file hashes. Reparse once on upgrade.
    if previous.is_empty() {
        transaction
            .execute("DELETE FROM symbols", [])
            .map_err(IndexError::Database)?;
    }
    {
        let mut delete_symbols = transaction
            .prepare("DELETE FROM symbols WHERE path = ?1")
            .map_err(IndexError::Database)?;
        let mut delete_file = transaction
            .prepare("DELETE FROM files WHERE path = ?1")
            .map_err(IndexError::Database)?;
        let mut save_file = transaction
            .prepare(
                "INSERT INTO files (path, hash) VALUES (?1, ?2)
                      ON CONFLICT(path) DO UPDATE SET hash = excluded.hash",
            )
            .map_err(IndexError::Database)?;
        let mut insert = transaction
            .prepare(
                "INSERT INTO symbols (path, name, kind, line, column, container) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(IndexError::Database)?;
        for path in &removed {
            delete_symbols
                .execute([path])
                .map_err(IndexError::Database)?;
            delete_file.execute([path]).map_err(IndexError::Database)?;
        }
        for file in &changed {
            delete_symbols
                .execute([&file.path])
                .map_err(IndexError::Database)?;
            for symbol in &file.symbols {
                insert
                    .execute(params![
                        file.path,
                        symbol.name,
                        symbol.kind.to_string(),
                        symbol.line as i64,
                        symbol.column as i64,
                        symbol.container,
                    ])
                    .map_err(IndexError::Database)?;
            }
            save_file
                .execute(params![file.path, file.hash])
                .map_err(IndexError::Database)?;
        }
    }
    let symbols_indexed: i64 = transaction
        .query_row("SELECT COUNT(*) FROM symbols", [], |row| row.get(0))
        .map_err(IndexError::Database)?;
    transaction.commit().map_err(IndexError::Database)?;

    Ok(IndexReport {
        files_parsed: changed.len(),
        files_unchanged: seen.len() - changed.len(),
        files_removed: removed.len(),
        symbols_indexed: symbols_indexed as usize,
        database_path,
    })
}

/// Search the stored index. This function does not read or parse source files.
pub fn find(root: &Path, query: &str) -> Result<Vec<Symbol>, IndexError> {
    let query = crate::search::Query::parse(query).map_err(IndexError::Query)?;
    let root = canonical_directory(root)?;
    let path = database_path(&root)?;
    if !path.is_file() {
        return Err(IndexError::MissingIndex(path));
    }

    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(IndexError::Database)?;
    let container = if has_container(&connection)? {
        "container"
    } else {
        "NULL"
    };
    let sql = format!(
        "SELECT path, name, kind, line, column, {container} FROM symbols ORDER BY path, line, column"
    );
    let mut statement = connection.prepare(&sql).map_err(IndexError::Database)?;
    let rows = statement
        .query_map([], |row| {
            let relative: String = row.get(0)?;
            let kind: String = row.get(2)?;
            let kind = match kind.as_str() {
                "function" => SymbolKind::Function,
                "method" => SymbolKind::Method,
                "type" => SymbolKind::Type,
                "trait" => SymbolKind::Trait,
                _ => return Err(rusqlite::Error::InvalidQuery),
            };
            Ok(Symbol {
                path: root.join(relative),
                name: row.get(1)?,
                kind,
                line: row.get::<_, i64>(3)? as usize,
                column: row.get::<_, i64>(4)? as usize,
                container: row.get(5)?,
            })
        })
        .map_err(IndexError::Database)?;
    let symbols = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(IndexError::Database)?;
    Ok(query.rank(symbols, &root))
}

fn has_container(connection: &Connection) -> Result<bool, IndexError> {
    let mut statement = connection
        .prepare("PRAGMA table_info(symbols)")
        .map_err(IndexError::Database)?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(IndexError::Database)?;
    for name in names {
        if name.map_err(IndexError::Database)? == "container" {
            return Ok(true);
        }
    }
    Ok(false)
}

fn canonical_directory(root: &Path) -> Result<PathBuf, IndexError> {
    let root = fs::canonicalize(root).map_err(IndexError::Io)?;
    if !root.is_dir() {
        return Err(IndexError::NotDirectory(root));
    }
    Ok(root)
}

fn database_path(root: &Path) -> Result<PathBuf, IndexError> {
    let home = std::env::var_os("HOME").ok_or(IndexError::MissingHome)?;
    let home = PathBuf::from(home);
    #[cfg(target_os = "macos")]
    let cache = home.join("Library/Caches");
    #[cfg(not(target_os = "macos"))]
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cache"));
    let key = blake3::hash(root.as_os_str().as_encoded_bytes());
    Ok(cache
        .join("scope")
        .join(key.to_hex().as_str())
        .join(INDEX_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn one_and_four_workers_match_scan_and_preserve_incremental_updates() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("scope-parallel-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();

        for jobs in [1, 4] {
            for i in 0..16 {
                fs::write(
                    root.join(format!("file_{i}.rs")),
                    format!(
                        "struct RustType{i};\nimpl RustType{i} {{ fn rust_method_{i}(&self) {{}} }}"
                    ),
                )
                .unwrap();
                fs::write(
                    root.join(format!("file_{i}.go")),
                    format!("package fixture\ntype GoType{i} struct {{}}\nfunc go_func_{i}() {{}}"),
                )
                .unwrap();
            }
            let expected = crate::scan(&root).unwrap();
            assert!(expected.warnings.is_empty());
            assert_eq!(expected.symbols.len(), 64);
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(jobs)
                .build()
                .unwrap();
            let update = || pool.install(|| rebuild(&root)).unwrap();

            let first = update();
            assert_eq!(first.files_parsed, 32);
            assert_eq!(first.symbols_indexed, 64);
            assert_eq!(find(&root, "").unwrap(), expected.symbols);
            let cached = update();
            assert_eq!((cached.files_parsed, cached.files_unchanged), (0, 32));

            fs::write(root.join("file_0.rs"), "fn renamed() {}").unwrap();
            fs::remove_file(root.join("file_0.go")).unwrap();
            let changed = update();
            assert_eq!(changed.files_parsed, 1);
            assert_eq!(changed.files_unchanged, 30);
            assert_eq!(changed.files_removed, 1);
            assert_eq!(changed.symbols_indexed, 61);
            assert_eq!(
                find(&root, "").unwrap(),
                crate::scan(&root).unwrap().symbols
            );
            assert_eq!(find(&root, "renamed").unwrap().len(), 1);

            // Both worker counts must start with a fresh index.
            fs::remove_dir_all(first.database_path.parent().unwrap()).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    // This fixture needs a filesystem that permits invalid UTF-8 filenames.
    #[cfg(target_os = "linux")]
    #[test]
    fn worker_failure_keeps_previous_symbols_and_hashes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("scope-worker-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("lib.rs");
        fs::write(&file, "fn persisted() {}").unwrap();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        let indexed = pool.install(|| rebuild(&root)).unwrap();

        fs::write(&file, "fn changed() {}").unwrap();
        let invalid = root.join(OsString::from_vec(b"invalid_\xff.rs".to_vec()));
        fs::write(&invalid, "fn other() {}").unwrap();
        assert!(matches!(
            pool.install(|| rebuild(&root)),
            Err(IndexError::NonUtf8Path(_))
        ));
        assert_eq!(find(&root, "persisted").unwrap().len(), 1);
        assert!(find(&root, "changed").unwrap().is_empty());

        fs::remove_file(invalid).unwrap();
        assert_eq!(pool.install(|| rebuild(&root)).unwrap().files_parsed, 1);
        assert_eq!(find(&root, "changed").unwrap().len(), 1);
        fs::remove_dir_all(indexed.database_path.parent().unwrap()).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reuses_unchanged_files_and_replaces_changed_or_deleted_symbols() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("scope-index-{}-{nonce}", std::process::id()));
        fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/lib.rs");
        fs::write(&file, "fn persisted() {}").unwrap();

        let indexed = rebuild(&root).unwrap();
        assert_eq!(indexed.files_parsed, 1);
        assert_eq!(indexed.symbols_indexed, 1);
        let repeated = rebuild(&root).unwrap();
        assert_eq!(repeated.files_parsed, 0);
        assert_eq!(repeated.files_unchanged, 1);

        fs::write(&file, "fn changed() {}").unwrap();
        let updated = rebuild(&root).unwrap();
        assert_eq!(updated.files_parsed, 1);
        assert!(find(&root, "persisted").unwrap().is_empty());
        assert_eq!(find(&root, "changed").unwrap().len(), 1);

        fs::remove_file(&file).unwrap();
        let saved = find(&root, "changed").unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "changed");

        let deleted = rebuild(&root).unwrap();
        assert_eq!(deleted.files_removed, 1);
        assert_eq!(deleted.symbols_indexed, 0);
        assert!(find(&root, "changed").unwrap().is_empty());
        fs::remove_dir_all(indexed.database_path.parent().unwrap()).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn incomplete_scan_keeps_previous_index() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("scope-failure-{}-{nonce}", std::process::id()));
        fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/lib.rs");
        fs::write(&file, "fn persisted() {}").unwrap();
        let indexed = rebuild(&root).unwrap();

        fs::write(&file, vec![b' '; MAX_SOURCE_BYTES as usize + 1]).unwrap();
        assert!(matches!(
            rebuild(&root),
            Err(IndexError::IncompleteScan(..))
        ));
        assert_eq!(find(&root, "persisted").unwrap().len(), 1);
        fs::remove_dir_all(indexed.database_path.parent().unwrap()).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn upgrades_old_symbol_only_index() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("scope-upgrade-{}-{nonce}", std::process::id()));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn current() {}").unwrap();
        let path = database_path(&root).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old = Connection::open(&path).unwrap();
        old.execute_batch(
            "CREATE TABLE symbols (path TEXT NOT NULL, name TEXT NOT NULL,
             kind TEXT NOT NULL, line INTEGER NOT NULL, column INTEGER NOT NULL);
             INSERT INTO symbols VALUES ('src/old.rs', 'stale', 'function', 1, 1);",
        )
        .unwrap();
        drop(old);

        let indexed = rebuild(&root).unwrap();
        assert_eq!(indexed.files_parsed, 1);
        assert_eq!(indexed.symbols_indexed, 1);
        assert!(find(&root, "stale").unwrap().is_empty());
        assert_eq!(find(&root, "current").unwrap().len(), 1);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }
}
