use code_scope::index;
use std::{env, error::Error, io, path::Path, process::Command};

pub fn run(root: &Path, query: &str) -> Result<(), Box<dyn Error>> {
    code_scope::search::Query::parse(query).map_err(io::Error::other)?;
    if query.trim().is_empty() {
        return crate::browse::run_query(root, query);
    }
    let root = root.canonicalize()?;
    index::rebuild(&root)?;
    let symbols = index::find(&root, query)?;
    match symbols.as_slice() {
        [] => Err(io::Error::other(format!("no symbols match {query:?}")).into()),
        [symbol] => Ok(open(&symbol.path, symbol.line, symbol.column)?),
        _ => crate::browse::run_query(&root, query),
    }
}

// Parse editor arguments without invoking a shell or expanding substitutions.
fn words(input: &str) -> io::Result<Vec<String>> {
    let mut result = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for ch in input.chars() {
        if escaped {
            word.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            started = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                word.push(ch);
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
            started = true;
        } else if ch.is_whitespace() {
            if started {
                result.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            word.push(ch);
            started = true;
        }
    }
    if quote.is_some() || escaped {
        return Err(io::Error::other(
            "unclosed quote or escape in VISUAL/EDITOR",
        ));
    }
    if started {
        result.push(word);
    }
    if result.first().is_none_or(String::is_empty) {
        return Err(io::Error::other("empty editor command"));
    }
    Ok(result)
}

fn command(editor: &str, path: &Path, line: usize, column: usize) -> io::Result<Command> {
    let args = words(editor)?;
    let mut command = Command::new(&args[0]);
    command.args(&args[1..]);
    let name = Path::new(&args[0])
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if matches!(name, "nvim" | "vim" | "vi" | "gvim" | "mvim") {
        if matches!(name, "gvim" | "mvim") {
            command.arg("-f");
        }
        command.arg(format!("+call cursor({},{})", line.max(1), column.max(1)));
        command.arg("--");
    }
    command.arg(path);
    Ok(command)
}

pub fn open(path: &Path, line: usize, column: usize) -> io::Result<()> {
    let editor = ["VISUAL", "EDITOR"]
        .into_iter()
        .filter_map(|key| env::var(key).ok())
        .find(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "nvim".into());
    let path = path.canonicalize()?;
    let status = command(&editor, &path, line, column)?
        .status()
        .map_err(|error| io::Error::other(format!("cannot launch {editor:?}: {error}")))?;
    if !status.success() {
        return Err(io::Error::other(format!("editor exited with {status}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[test]
    fn open_without_query_parses() {
        let cli = crate::Cli::try_parse_from(["scope", "open"]).unwrap();
        assert!(
            matches!(cli.command, crate::Command::Open { query, path: None } if query.is_empty())
        );
    }
    #[test]
    fn editor_arguments_are_literal_and_byte_column_is_preserved() {
        let cmd = command(
            "'/opt/My Editor/nvim' -u 'my config'",
            Path::new("/repo/a $(echo bad).rs"),
            4,
            9,
        )
        .unwrap();
        let args: Vec<_> = cmd.get_args().map(|s| s.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "-u",
                "my config",
                "+call cursor(4,9)",
                "--",
                "/repo/a $(echo bad).rs"
            ]
        );
        assert!(words("vim 'unfinished").is_err());
        assert!(words("''").is_err());
    }
}
