use code_scope::{Language, SymbolKind, extract_symbols, index, search::Query};
use rusqlite::Connection;
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

struct Fixture {
    root: PathBuf,
    database: Option<PathBuf>,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("scope-search-{}-{nonce}", std::process::id()));
        fs::create_dir_all(root.join("internal")).unwrap();
        Self {
            root: root.canonicalize().unwrap(),
            database: None,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        if let Some(path) = &self.database {
            let _ = fs::remove_dir_all(path.parent().unwrap());
        }
    }
}

#[test]
fn rust_context_includes_impl_trait_module_and_nested_function() {
    let source = b"mod db { struct Store; trait Read { fn get(&self); } impl Read for Store { fn get(&self) { fn helper() {} } } }";
    let symbols = extract_symbols(std::path::Path::new("lib.rs"), source, Language::Rust).unwrap();
    let names: Vec<_> = symbols.iter().map(|s| s.display_name()).collect();
    assert!(names.contains(&"db::Read::get".to_owned()), "{names:?}");
    assert!(
        names.contains(&"db::Store as Read::get".to_owned()),
        "{names:?}"
    );
    let helper = symbols.iter().find(|s| s.name == "helper").unwrap();
    assert_eq!(helper.kind, SymbolKind::Function);
    assert_eq!(helper.container.as_deref(), Some("db::Store as Read::get"));
}

#[test]
fn search_preserves_receiver_metadata_and_matches_memory_ranking() {
    let mut fixture = Fixture::new();
    let source = "package sample\ntype Engine struct{}\nfunc (e *Engine) EngineTransaction() {}\nfunc (e Engine) Commit() {}\nfunc EngineTransactionFactory() {}\n";
    let file = fixture.root.join("internal/engine.go");
    fs::write(&file, source).unwrap();

    let report = index::rebuild(&fixture.root).unwrap();
    fixture.database = Some(report.database_path);

    let direct = extract_symbols(&file, source.as_bytes(), Language::Go).unwrap();
    let query = "engtxn kind:method path:internal";
    let stored = index::find(&fixture.root, query).unwrap();
    let memory = Query::parse(query).unwrap().rank(direct, &fixture.root);

    assert_eq!(stored, memory);
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].display_name(), "*Engine::EngineTransaction");
    assert_eq!(
        index::find(&fixture.root, "EngineTransaction").unwrap()[0].name,
        "EngineTransaction"
    );
    assert!(index::find(&fixture.root, "kind:trait").unwrap().is_empty());
    assert!(index::find(&fixture.root, "kind:invalid").is_err());
}

#[test]
fn legacy_hash_cache_reparses_once_and_migration_rolls_back_on_failure() {
    let mut fixture = Fixture::new();
    let file = fixture.root.join("internal/store.rs");
    fs::write(&file, "struct Store; impl Store { fn open() {} }").unwrap();

    let report = index::rebuild(&fixture.root).unwrap();
    fixture.database = Some(report.database_path.clone());
    {
        let db = Connection::open(&report.database_path).unwrap();
        db.execute_batch("ALTER TABLE symbols DROP COLUMN container;")
            .unwrap();
    }

    let oversized = fixture.root.join("large.rs");
    fs::write(&oversized, vec![b' '; 2 * 1024 * 1024 + 1]).unwrap();
    assert!(index::rebuild(&fixture.root).is_err());

    {
        let db = Connection::open(&report.database_path).unwrap();
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert!(db.prepare("SELECT container FROM symbols").is_err());
    }

    fs::remove_file(oversized).unwrap();
    assert_eq!(index::rebuild(&fixture.root).unwrap().files_parsed, 1);
    assert_eq!(
        index::find(&fixture.root, "open").unwrap()[0]
            .container
            .as_deref(),
        Some("Store")
    );
    assert_eq!(index::rebuild(&fixture.root).unwrap().files_parsed, 0);
}
