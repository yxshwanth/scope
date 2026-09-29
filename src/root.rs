use std::io;
use std::path::{Path, PathBuf};

pub fn resolve(explicit: Option<&Path>) -> io::Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    Ok(discover(&std::env::current_dir()?))
}

fn discover(start: &Path) -> PathBuf {
    for directory in start.ancestors() {
        let marker = directory.join(".git");
        if marker.is_dir() || marker.is_file() {
            return directory.to_path_buf();
        }
    }
    start.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("scope-root-{}-{nonce}", std::process::id()));
            fs::create_dir_all(path.join("internal/engine")).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn detects_git_directory_from_nested_directory() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join(".git")).unwrap();

        assert_eq!(discover(&fixture.0.join("internal/engine")), fixture.0);
    }

    #[test]
    fn detects_git_file_and_prefers_nearest_repository() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join(".git")).unwrap();

        let nested = fixture.0.join("internal");
        fs::write(nested.join(".git"), "gitdir: /example/worktrees/engine\n").unwrap();

        assert_eq!(discover(&nested.join("engine")), nested);
    }

    #[test]
    fn falls_back_when_no_git_marker_exists() {
        let fixture = Fixture::new();
        let path = fixture.0.join("internal/engine");

        assert_eq!(discover(&path), path);
    }

    #[test]
    fn explicit_paths_are_preserved() {
        for path in [".", "internal/engine", "/tmp/explicit-scope-root"] {
            assert_eq!(resolve(Some(Path::new(path))).unwrap(), Path::new(path));
        }
    }

    #[test]
    fn cli_distinguishes_omitted_path_from_explicit_dot() {
        let cli = crate::Cli::try_parse_from(["scope", "find", "Engine"]).unwrap();

        assert!(matches!(
            cli.command,
            crate::Command::Find { path: None, .. }
        ));

        let cli = crate::Cli::try_parse_from(["scope", "find", "Engine", "."]).unwrap();

        match cli.command {
            crate::Command::Find {
                path: Some(path), ..
            } => {
                assert_eq!(path, Path::new("."));
            }
            _ => panic!("explicit path was not preserved"),
        }
    }
}
