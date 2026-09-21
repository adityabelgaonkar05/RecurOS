//! Locating and initialising the RecurOS home directory (`~/ctx`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum HomeError {
    #[error("cannot determine home directory; set CTX_HOME")]
    NoHome,
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> HomeError + '_ {
    move |source| HomeError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Lines `.gitignore` must contain. `.machine` is critical: it names this
/// machine's shard, and if it were committed a second machine would pull it
/// and start writing into the first machine's shard (breaking invariant 3).
const GITIGNORE_REQUIRED: &[&str] = &[".cache/", ".machine", ".node/"];

// `merge=union` is belt-and-braces: sharding already makes concurrent appends
// land in different files, but if two clones of the same machine ever do touch
// one shard, a union merge keeps both sides' lines rather than conflicting.
const GITATTRIBUTES: &str = "\
* text=auto eol=lf
log/**/*.jsonl merge=union
";

/// The RecurOS home: a single private git repo holding the log.
#[derive(Debug, Clone)]
pub struct CtxHome {
    root: PathBuf,
}

impl CtxHome {
    /// `$CTX_HOME` if set (tests, multiple stores), otherwise `~/ctx`.
    pub fn locate() -> Result<Self, HomeError> {
        if let Some(p) = std::env::var_os("CTX_HOME").filter(|p| !p.is_empty()) {
            return Ok(Self::at(PathBuf::from(p)));
        }
        let home = std::env::home_dir().ok_or(HomeError::NoHome)?;
        Ok(Self::at(home.join("ctx")))
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        CtxHome { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn log_dir(&self) -> PathBuf {
        self.root.join("log")
    }

    pub fn refs_dir(&self) -> PathBuf {
        self.root.join("refs")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.join(".cache")
    }

    pub fn db_path(&self) -> PathBuf {
        self.cache_dir().join("ctx.db")
    }

    pub fn exists(&self) -> bool {
        self.log_dir().is_dir()
    }

    /// Create the layout if missing. Idempotent, and never overwrites files
    /// the user may have edited. Returns true if the home was newly created.
    ///
    /// `git init` is attempted but is not fatal: everything works without git
    /// (spec invariant 8), git only adds sync.
    pub fn ensure(&self) -> Result<bool, HomeError> {
        let created = !self.exists();
        for dir in [
            self.log_dir(),
            self.refs_dir(),
            self.cache_dir(),
            self.root.join("packs"),
        ] {
            fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        }
        ensure_lines(&self.root.join(".gitignore"), GITIGNORE_REQUIRED)?;
        write_if_missing(&self.root.join(".gitattributes"), GITATTRIBUTES)?;
        if !self.root.join(".git").exists() {
            // Ignore failure (git not installed): the store is still usable.
            let _ = Command::new("git")
                .args(["init", "--quiet", "--initial-branch=main"])
                .current_dir(&self.root)
                .status();
        }
        Ok(created)
    }

    /// This machine's stable shard name. Derived from the hostname on first
    /// use and then persisted in `.machine`, so renaming the host later does
    /// not fork the shard (which would break the one-writer-per-shard rule's
    /// bookkeeping, though not correctness).
    pub fn machine_id(&self) -> Result<String, HomeError> {
        let path = self.root.join(".machine");
        match fs::read_to_string(&path) {
            Ok(s) if !s.trim().is_empty() => return Ok(s.trim().to_owned()),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&path)(e)),
        }
        let id = machine_slug(&gethostname::gethostname().to_string_lossy());
        fs::write(&path, format!("{id}\n")).map_err(io_err(&path))?;
        Ok(id)
    }
}

/// Append any of `required` not already present as a line, keeping the
/// user's own entries.
fn ensure_lines(path: &Path, required: &[&str]) -> Result<(), HomeError> {
    let existing = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(io_err(path)(e)),
    };
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|req| !existing.lines().any(|l| l.trim() == *req))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for m in missing {
        out.push_str(m);
        out.push('\n');
    }
    fs::write(path, out).map_err(io_err(path))
}

fn write_if_missing(path: &Path, contents: &str) -> Result<(), HomeError> {
    if !path.exists() {
        fs::write(path, contents).map_err(io_err(path))?;
    }
    Ok(())
}

/// Turn a hostname into a path-safe shard name. `cloud` is reserved for the
/// Worker's shard, so a host literally named that gets a suffix.
pub fn machine_slug(hostname: &str) -> String {
    let mut slug = String::new();
    for c in hostname.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    match slug {
        "" => "machine".to_owned(),
        "cloud" => "cloud-host".to_owned(),
        s => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(machine_slug("DESKTOP-AB12.local"), "desktop-ab12-local");
        assert_eq!(machine_slug("  Shrey's MacBook  "), "shrey-s-macbook");
        assert_eq!(machine_slug("???"), "machine");
        assert_eq!(machine_slug("Cloud"), "cloud-host");
    }

    #[test]
    fn ensure_is_idempotent_and_machine_id_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let home = CtxHome::at(dir.path().join("ctx"));
        assert!(home.ensure().unwrap());
        fs::write(home.root().join(".gitignore"), "custom\n.cache/").unwrap();
        assert!(!home.ensure().unwrap());
        assert_eq!(
            fs::read_to_string(home.root().join(".gitignore")).unwrap(),
            "custom\n.cache/\n.machine\n.node/\n",
            "user lines kept, missing required lines appended"
        );
        let id = home.machine_id().unwrap();
        assert_eq!(home.machine_id().unwrap(), id);
    }
}
