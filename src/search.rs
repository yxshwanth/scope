//! Shared query parsing, filtering, and ranking for every UI.
use crate::{Symbol, SymbolKind};
use std::path::Path;

#[derive(Debug, Default)]
pub struct Query {
    terms: Vec<String>,
    kinds: Vec<SymbolKind>,
    paths: Vec<String>,
}

impl Query {
    pub fn parse(input: &str) -> Result<Self, String> {
        if input.chars().count() > 512 {
            return Err("query is limited to 512 characters".into());
        }
        let mut query = Self::default();
        for token in tokenize(input)? {
            if let Some(value) = token.strip_prefix("kind:") {
                for kind in value.split(',') {
                    query.kinds.push(match kind {
                        "function" | "fn" => SymbolKind::Function,
                        "method" => SymbolKind::Method,
                        "type" => SymbolKind::Type,
                        "trait" => SymbolKind::Trait,
                        _ => {
                            return Err(format!(
                                "unknown kind '{kind}'; use function, method, type, or trait"
                            ));
                        }
                    });
                }
            } else if let Some(value) = token.strip_prefix("path:") {
                if value.is_empty() {
                    return Err("path: needs a path fragment".into());
                }
                query.paths.push(value.to_lowercase());
            } else if !token.is_empty() {
                query.terms.push(token.to_lowercase());
            }
        }
        Ok(query)
    }

    pub fn rank_indices(&self, symbols: &[Symbol], root: &Path) -> Vec<usize> {
        let mut ranked: Vec<_> = symbols
            .iter()
            .enumerate()
            .filter_map(|(i, symbol)| self.score(symbol, root).map(|score| (score, i)))
            .collect();
        ranked.sort_by(|(a, i), (b, j)| {
            let left = &symbols[*i];
            let right = &symbols[*j];
            a.cmp(b)
                .then(left.path.cmp(&right.path))
                .then(left.line.cmp(&right.line))
                .then(left.column.cmp(&right.column))
                .then(left.name.cmp(&right.name))
                .then(left.container.cmp(&right.container))
        });
        ranked.into_iter().map(|(_, i)| i).collect()
    }

    pub fn rank(&self, symbols: Vec<Symbol>, root: &Path) -> Vec<Symbol> {
        let indices = self.rank_indices(&symbols, root);
        let mut slots: Vec<_> = symbols.into_iter().map(Some).collect();
        indices
            .into_iter()
            .map(|i| slots[i].take().unwrap())
            .collect()
    }

    fn score(&self, symbol: &Symbol, root: &Path) -> Option<(usize, usize, usize, usize)> {
        if !self.kinds.is_empty() && !self.kinds.contains(&symbol.kind) {
            return None;
        }
        if !self.paths.is_empty() {
            let path = symbol
                .path
                .strip_prefix(root)
                .unwrap_or(&symbol.path)
                .to_string_lossy()
                .replace('\\', "/")
                .to_lowercase();
            if !self.paths.iter().all(|fragment| path.contains(fragment)) {
                return None;
            }
        }
        let name = symbol.name.to_lowercase();
        let context = symbol.container.as_deref().unwrap_or("").to_lowercase();
        let mut score = (0usize, 0usize, 0usize, 0usize);
        for term in &self.terms {
            let quality = quality(&name, term)
                .or_else(|| quality(&context, term).map(|(_, penalty)| (5, penalty)))?;
            score.0 = score.0.max(quality.0);
            score.1 += quality.0;
            score.2 += quality.1;
        }
        if !self.terms.is_empty() {
            score.3 = name.chars().count();
        }
        Some(score)
    }
}

fn quality(name: &str, term: &str) -> Option<(usize, usize)> {
    if name == term {
        return Some((0, 0));
    }
    if name.starts_with(term) {
        return Some((1, 0));
    }
    if let Some(start) = name.find(term) {
        return Some((2, name[..start].chars().count()));
    }
    let name: Vec<_> = name.chars().collect();
    let term: Vec<_> = term.chars().collect();
    if let Some(penalty) = subsequence(&name, &term) {
        return Some((3, penalty));
    }
    // Bounded tolerance for one extra/mistyped query character.
    if (5..=32).contains(&term.len()) {
        return (0..term.len())
            .filter_map(|skip| {
                let shorter: Vec<_> = term
                    .iter()
                    .enumerate()
                    .filter_map(|(i, c)| (i != skip).then_some(*c))
                    .collect();
                subsequence(&name, &shorter)
            })
            .min()
            .map(|penalty| (4, penalty));
    }
    None
}

fn subsequence(name: &[char], term: &[char]) -> Option<usize> {
    let mut next = 0;
    let mut first = None;
    let mut last = 0;
    for (i, character) in name.iter().enumerate() {
        if term.get(next) == Some(character) {
            first.get_or_insert(i);
            last = i;
            next += 1;
            if next == term.len() {
                break;
            }
        }
    }
    if next != term.len() {
        return None;
    }
    let first = first?;
    Some(first + (last - first + 1 - term.len()) * 2)
}

fn tokenize(input: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut escaped = false;
    for c in input.chars() {
        if escaped {
            token.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if let Some(end) = quote {
            if c == end {
                quote = None;
            } else {
                token.push(c);
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c.is_whitespace() {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        } else {
            token.push(c);
        }
    }
    if escaped || quote.is_some() {
        return Err("finish the quoted or escaped query".into());
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(name: &str) -> Symbol {
        Symbol {
            name: name.into(),
            kind: SymbolKind::Method,
            path: "/repo/internal/engine.go".into(),
            line: 1,
            column: 1,
            container: Some("*Store".into()),
        }
    }

    #[test]
    fn exact_prefix_substring_fuzzy_and_typo_order() {
        let items = ["EnterNewGame", "my_eng", "Engine", "eng"]
            .into_iter()
            .map(symbol)
            .collect();
        let result = Query::parse("eng").unwrap().rank(items, Path::new("/repo"));
        assert_eq!(
            result.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["eng", "Engine", "my_eng", "EnterNewGame"]
        );
        assert!(quality("enginetransaction", "engtxn").is_some());
        assert!(quality("enginetransaction", "zzzz").is_none());
        assert_eq!(
            quality("café", "CAFÉ".to_lowercase().as_str()),
            Some((0, 0))
        );
    }

    #[test]
    fn filters_context_quotes_and_errors() {
        let items = vec![symbol("Open"), symbol("Close")];
        let query = Query::parse("Store op kind:method path:internal").unwrap();
        assert_eq!(query.rank_indices(&items, Path::new("/repo")), [0]);
        assert!(
            Query::parse("kind:type")
                .unwrap()
                .rank_indices(&items, Path::new("/repo"))
                .is_empty()
        );
        let mut spaced = symbol("Open");
        spaced.path = "/repo/my files/store.rs".into();
        assert_eq!(
            Query::parse("path:\"my files\" kind:fn,method")
                .unwrap()
                .rank_indices(&[spaced], Path::new("/repo")),
            [0]
        );
        for invalid in ["kind:wat", "path:", "path:\"unfinished"] {
            assert!(Query::parse(invalid).is_err());
        }
    }
}
