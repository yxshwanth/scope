use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use tree_sitter::{Node, Parser};

pub mod index;
pub mod search;

const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Language {
    Rust,
    Go,
}

impl Language {
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "rs" => Some(Self::Rust),
            "go" => Some(Self::Go),
            _ => None,
        }
    }

    fn grammar(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymbolKind {
    Function,
    Method,
    Type,
    Trait,
}

impl fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Type => "type",
            Self::Trait => "trait",
        };
        f.write_str(label)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct Symbol {
    pub container: Option<String>,
    pub name: String,
    pub kind: SymbolKind,
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
}

#[derive(Debug)]
pub struct ScanWarning {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct ScanReport {
    pub files_parsed: usize,
    pub symbols: Vec<Symbol>,
    pub warnings: Vec<ScanWarning>,
}

pub(crate) struct SourceDiscovery {
    pub paths: Vec<(PathBuf, Language)>,
    pub warnings: Vec<ScanWarning>,
}

#[derive(Debug)]
pub enum ScanError {
    Io(std::io::Error),
    Grammar(String),
    ParseCanceled,
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "cannot open scan path: {error}"),
            Self::Grammar(error) => write!(f, "cannot load language grammar: {error}"),
            Self::ParseCanceled => f.write_str("parser did not produce a syntax tree"),
        }
    }
}

impl Error for ScanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

/// Parse a single source file into owned symbol records.
///
/// The records own their names, so they remain usable after the source bytes
/// and Tree-sitter syntax tree are dropped.
pub fn extract_symbols(
    path: &Path,
    source: &[u8],
    language: Language,
) -> Result<Vec<Symbol>, ScanError> {
    let mut parser = Parser::new();
    parser
        .set_language(&language.grammar())
        .map_err(|error| ScanError::Grammar(error.to_string()))?;
    let tree = parser.parse(source, None).ok_or(ScanError::ParseCanceled)?;

    let mut symbols = Vec::new();
    visit(tree.root_node(), source, language, path, &mut symbols);
    Ok(symbols)
}

fn visit(
    node: Node<'_>,
    source: &[u8],
    language: Language,
    path: &Path,
    symbols: &mut Vec<Symbol>,
) {
    if let Some(kind) = symbol_kind(node, language)
        && let Some(name_node) = node.child_by_field_name("name")
        && let Ok(name) = name_node.utf8_text(source)
    {
        let start = name_node.start_position();
        symbols.push(Symbol {
            container: container_name(node, source, language),
            name: name.to_owned(),
            kind,
            path: path.to_path_buf(),
            line: start.row + 1,
            column: start.column + 1,
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, source, language, path, symbols);
    }
}

fn symbol_kind(node: Node<'_>, language: Language) -> Option<SymbolKind> {
    match (language, node.kind()) {
        (Language::Rust, "function_item" | "function_signature_item") => {
            if is_rust_method(node) {
                Some(SymbolKind::Method)
            } else {
                Some(SymbolKind::Function)
            }
        }
        (Language::Rust, "struct_item" | "enum_item" | "type_item" | "union_item") => {
            Some(SymbolKind::Type)
        }
        (Language::Rust, "trait_item") => Some(SymbolKind::Trait),
        (Language::Go, "function_declaration") => Some(SymbolKind::Function),
        (Language::Go, "method_declaration") => Some(SymbolKind::Method),
        (Language::Go, "type_spec" | "type_alias") => Some(SymbolKind::Type),
        _ => None,
    }
}

impl Symbol {
    pub fn display_name(&self) -> String {
        match self.container.as_deref() {
            Some(container) => format!("{container}::{}", self.name),
            None => self.name.clone(),
        }
    }
}

fn node_text(node: Node<'_>, source: &[u8]) -> Option<String> {
    node.utf8_text(source)
        .ok()
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn container_name(node: Node<'_>, source: &[u8], language: Language) -> Option<String> {
    if language == Language::Go {
        let receiver = node.child_by_field_name("receiver")?;
        let mut cursor = receiver.walk();
        for parameter in receiver.named_children(&mut cursor) {
            if let Some(kind) = parameter.child_by_field_name("type") {
                return node_text(kind, source);
            }
        }
        return None;
    }
    let mut scopes = Vec::new();
    let mut parent = node.parent();
    while let Some(ancestor) = parent {
        let label = match ancestor.kind() {
            "impl_item" => ancestor.child_by_field_name("type").and_then(|ty| {
                let ty = node_text(ty, source)?;
                Some(
                    match ancestor
                        .child_by_field_name("trait")
                        .and_then(|n| node_text(n, source))
                    {
                        Some(trait_name) => format!("{ty} as {trait_name}"),
                        None => ty,
                    },
                )
            }),
            "trait_item" | "mod_item" | "function_item" => ancestor
                .child_by_field_name("name")
                .and_then(|name| node_text(name, source)),
            _ => None,
        };
        if let Some(label) = label {
            scopes.push(label);
        }
        parent = ancestor.parent();
    }
    scopes.reverse();
    (!scopes.is_empty()).then(|| scopes.join("::"))
}

fn is_rust_method(node: Node<'_>) -> bool {
    let mut parent = node.parent();
    while let Some(ancestor) = parent {
        match ancestor.kind() {
            "impl_item" | "trait_item" => return true,
            "function_item" => return false,
            _ => {}
        }
        parent = ancestor.parent();
    }
    false
}

/// Scan a file or directory, respecting standard gitignore filters.
/// Individual unreadable files are reported as warnings so other files remain searchable.
pub fn scan(root: &Path) -> Result<ScanReport, ScanError> {
    let discovery = discover_sources(root)?;
    let mut report = ScanReport {
        warnings: discovery.warnings,
        ..ScanReport::default()
    };
    for (path, language) in discovery.paths {
        match fs::read(&path) {
            Ok(source) => match extract_symbols(&path, &source, language) {
                Ok(symbols) => {
                    report.files_parsed += 1;
                    report.symbols.extend(symbols);
                }
                Err(error) => report.warnings.push(ScanWarning {
                    path,
                    message: error.to_string(),
                }),
            },
            Err(error) => report.warnings.push(ScanWarning {
                path,
                message: error.to_string(),
            }),
        }
    }
    report.symbols.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.line.cmp(&right.line))
            .then(left.column.cmp(&right.column))
    });
    Ok(report)
}

pub(crate) fn discover_sources(root: &Path) -> Result<SourceDiscovery, ScanError> {
    let metadata = fs::metadata(root).map_err(ScanError::Io)?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(ScanError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path is neither a regular file nor a directory",
        )));
    }

    let mut discovery = SourceDiscovery {
        paths: Vec::new(),
        warnings: Vec::new(),
    };
    let mut walker = WalkBuilder::new(root);
    walker.standard_filters(true);
    // A repository may have no .gitignore. Build output is never source code.
    // Keep the explicit root so `scope scan path/to/target` remains possible.
    walker.filter_entry(|entry| {
        entry.depth() == 0
            || !entry.file_type().is_some_and(|kind| kind.is_dir())
            || !matches!(
                entry.file_name().to_str(),
                Some("target" | ".git" | "node_modules")
            )
    });

    for result in walker.build() {
        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                discovery.warnings.push(ScanWarning {
                    path: root.to_path_buf(),
                    message: error.to_string(),
                });
                continue;
            }
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        let Some(language) = Language::from_path(path) else {
            continue;
        };
        match fs::metadata(path) {
            Ok(metadata) if metadata.len() > MAX_SOURCE_BYTES => {
                discovery.warnings.push(ScanWarning {
                    path: path.to_path_buf(),
                    message: "source file exceeds the 2 MiB first-slice limit".to_owned(),
                });
                continue;
            }
            Err(error) => {
                discovery.warnings.push(ScanWarning {
                    path: path.to_path_buf(),
                    message: error.to_string(),
                });
                continue;
            }
            _ => {}
        }
        discovery.paths.push((path.to_path_buf(), language));
    }
    Ok(discovery)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_functions_methods_and_types() {
        let source = br#"
            struct Store;
            trait Reader { fn read(&self); }
            impl Store { fn open() {} }
            fn run() {}
        "#;
        let symbols = extract_symbols(Path::new("store.rs"), source, Language::Rust).unwrap();
        let found: Vec<_> = symbols
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.kind))
            .collect();
        assert_eq!(
            found,
            [
                ("Store", SymbolKind::Type),
                ("Reader", SymbolKind::Trait),
                ("read", SymbolKind::Method),
                ("open", SymbolKind::Method),
                ("run", SymbolKind::Function),
            ]
        );
    }

    #[test]
    fn extracts_go_functions_methods_and_types() {
        let source = br#"
            package example
            type Store struct{}
            func (s *Store) Open() {}
            func Run() {}
        "#;
        let symbols = extract_symbols(Path::new("store.go"), source, Language::Go).unwrap();
        let found: Vec<_> = symbols
            .iter()
            .map(|symbol| (symbol.name.as_str(), symbol.kind))
            .collect();
        assert_eq!(
            found,
            [
                ("Store", SymbolKind::Type),
                ("Open", SymbolKind::Method),
                ("Run", SymbolKind::Function),
            ]
        );
    }

    #[test]
    fn skips_cargo_output_even_without_gitignore() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("code-scope-{}-{nonce}", std::process::id()));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target/debug/build/generated")).unwrap();
        fs::write(root.join("src/main.rs"), "fn app() {}").unwrap();
        fs::write(
            root.join("target/debug/build/generated/bindgen.rs"),
            "fn generated() {}",
        )
        .unwrap();

        let report = scan(&root).unwrap();
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(report.files_parsed, 1);
        assert_eq!(report.symbols.len(), 1);
        assert_eq!(report.symbols[0].name, "app");
    }
}
