//! Discovery of grind prompt files across project, global, and override roots.
//!
//! Three sources are recognised, in decreasing precedence:
//!
//! 1. `--prompts-dir <path>` (override). When set, project and global sources
//!    are skipped entirely.
//! 2. `./.pitboss/prompts/` under the project root.
//! 3. `~/.pitboss/prompts/` in the user's home directory.
//!
//! Project entries shadow global entries that share a `name`. Within a single
//! directory, files are walked in sorted filename order so duplicate-name
//! collisions resolve deterministically (first-by-filename wins).
//!
//! Per-file parse failures are collected into [`DiscoveryResult::errors`] so
//! callers like `pitboss prompts validate` can report every bad file in one
//! pass instead of stopping at the first.
//!
//! No recursion: only `*.md` files at the top level of each directory are
//! considered. Missing directories are not errors — they yield an empty
//! contribution.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use super::prompt::{parse_prompt_str, PromptDoc, PromptParseError, PromptSource};

/// Inputs to [`discover_prompts`]. All paths are absolute or relative to the
/// caller's current working directory; this module does not canonicalise.
#[derive(Debug, Clone)]
pub struct DiscoveryOptions {
    /// Repository root. The project source is `<project_root>/.pitboss/prompts/`.
    pub project_root: PathBuf,
    /// Optional home directory. The global source is
    /// `<home_dir>/.pitboss/prompts/`. `None` disables the global source.
    pub home_dir: Option<PathBuf>,
    /// When `Some`, only this directory is consulted; project and global
    /// sources are ignored.
    pub override_dir: Option<PathBuf>,
}

/// Result of a discovery pass: the surviving prompts plus a list of files that
/// failed to read or parse, in walk order.
#[derive(Debug)]
pub struct DiscoveryResult {
    /// Prompts in deterministic order (sorted by `meta.name`).
    pub prompts: Vec<PromptDoc>,
    /// Files that failed to load, paired with their error.
    pub errors: Vec<(PathBuf, PromptParseError)>,
}

/// Resolve the conventional global prompts directory from the `HOME` env var.
///
/// Returns `None` if `HOME` is unset or empty. Intentionally does not depend on
/// `dirs`/`home`/etc.; pitboss treats `HOME` as the contract.
pub fn resolve_home_prompts_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".pitboss").join("prompts"))
}

/// Walk the configured sources and return discovered prompts plus any per-file
/// errors. See module docs for precedence rules.
pub fn discover_prompts(opts: DiscoveryOptions) -> DiscoveryResult {
    let mut by_name: BTreeMap<String, PromptDoc> = BTreeMap::new();
    let mut errors: Vec<(PathBuf, PromptParseError)> = Vec::new();

    if let Some(override_dir) = opts.override_dir.as_deref() {
        load_dir(
            override_dir,
            PromptSource::Override,
            &mut by_name,
            &mut errors,
        );
    } else {
        let project_dir = opts.project_root.join(".pitboss").join("prompts");
        load_dir(
            &project_dir,
            PromptSource::Project,
            &mut by_name,
            &mut errors,
        );
        if let Some(home) = opts.home_dir.as_deref() {
            let global_dir = home.join(".pitboss").join("prompts");
            load_dir(
                &global_dir,
                PromptSource::Global,
                &mut by_name,
                &mut errors,
            );
        }
    }

    DiscoveryResult {
        prompts: by_name.into_values().collect(),
        errors,
    }
}

fn load_dir(
    dir: &Path,
    source: PromptSource,
    by_name: &mut BTreeMap<String, PromptDoc>,
    errors: &mut Vec<(PathBuf, PromptParseError)>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        // Missing or unreadable directories contribute nothing. A surfacing
        // pass at the CLI layer (phase 03) can warn separately if it cares.
        Err(_) => return,
    };

    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(OsStr::to_str) != Some("md") {
            continue;
        }
        paths.push(path);
    }
    paths.sort();

    for path in paths {
        match read_and_parse(&path, source) {
            Ok(doc) => {
                // Higher-precedence sources are loaded first, so a name that's
                // already present should not be overwritten — that's how
                // project shadows global, and how within-directory duplicates
                // resolve to the alphabetically-first file.
                by_name.entry(doc.meta.name.clone()).or_insert(doc);
            }
            Err(e) => errors.push((path, e)),
        }
    }
}

fn read_and_parse(path: &Path, source: PromptSource) -> Result<PromptDoc, PromptParseError> {
    let display = path.display().to_string();
    let raw = fs::read_to_string(path).map_err(|e| PromptParseError::Io {
        path: display,
        source: e,
    })?;
    parse_prompt_str(&raw, path, source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_prompt(dir: &Path, file_name: &str, name: &str, body: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(file_name);
        let content = format!("---\nname: {name}\ndescription: test prompt\n---\n{body}");
        fs::write(&path, content).unwrap();
        path
    }

    fn write_raw(dir: &Path, file_name: &str, content: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(file_name);
        fs::write(&path, content).unwrap();
        path
    }

    fn project_dir(root: &Path) -> PathBuf {
        root.join(".pitboss").join("prompts")
    }

    fn global_dir(home: &Path) -> PathBuf {
        home.join(".pitboss").join("prompts")
    }

    #[test]
    fn project_only_sources_are_loaded() {
        let root = TempDir::new().unwrap();
        write_prompt(&project_dir(root.path()), "alpha.md", "alpha", "hi");
        write_prompt(&project_dir(root.path()), "bravo.md", "bravo", "hi");

        let res = discover_prompts(DiscoveryOptions {
            project_root: root.path().to_path_buf(),
            home_dir: None,
            override_dir: None,
        });

        assert!(res.errors.is_empty());
        let names: Vec<&str> = res.prompts.iter().map(|p| p.meta.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "bravo"]);
        assert!(res
            .prompts
            .iter()
            .all(|p| p.source_kind == PromptSource::Project));
    }

    #[test]
    fn global_only_sources_are_loaded_when_project_is_empty() {
        let root = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        write_prompt(&global_dir(home.path()), "triage.md", "triage", "hi");

        let res = discover_prompts(DiscoveryOptions {
            project_root: root.path().to_path_buf(),
            home_dir: Some(home.path().to_path_buf()),
            override_dir: None,
        });

        assert!(res.errors.is_empty());
        assert_eq!(res.prompts.len(), 1);
        assert_eq!(res.prompts[0].meta.name, "triage");
        assert_eq!(res.prompts[0].source_kind, PromptSource::Global);
    }

    #[test]
    fn project_shadows_global_for_same_name() {
        let root = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let project_path =
            write_prompt(&project_dir(root.path()), "fp.md", "fp-hunter", "project body");
        write_prompt(&global_dir(home.path()), "fp.md", "fp-hunter", "global body");
        // Distinct global-only entry survives.
        write_prompt(&global_dir(home.path()), "lint.md", "lint", "global body");

        let res = discover_prompts(DiscoveryOptions {
            project_root: root.path().to_path_buf(),
            home_dir: Some(home.path().to_path_buf()),
            override_dir: None,
        });

        assert!(res.errors.is_empty());
        let names: Vec<&str> = res.prompts.iter().map(|p| p.meta.name.as_str()).collect();
        assert_eq!(names, vec!["fp-hunter", "lint"]);
        let fp = res
            .prompts
            .iter()
            .find(|p| p.meta.name == "fp-hunter")
            .unwrap();
        assert_eq!(fp.source_kind, PromptSource::Project);
        assert_eq!(fp.source_path, project_path);
        assert!(fp.body.contains("project body"));
        let lint = res.prompts.iter().find(|p| p.meta.name == "lint").unwrap();
        assert_eq!(lint.source_kind, PromptSource::Global);
    }

    #[test]
    fn override_dir_replaces_project_and_global_entirely() {
        let root = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let override_root = TempDir::new().unwrap();

        write_prompt(&project_dir(root.path()), "p.md", "from-project", "x");
        write_prompt(&global_dir(home.path()), "g.md", "from-global", "x");
        let override_path = write_prompt(override_root.path(), "o.md", "from-override", "x");

        let res = discover_prompts(DiscoveryOptions {
            project_root: root.path().to_path_buf(),
            home_dir: Some(home.path().to_path_buf()),
            override_dir: Some(override_root.path().to_path_buf()),
        });

        assert!(res.errors.is_empty());
        assert_eq!(res.prompts.len(), 1);
        let only = &res.prompts[0];
        assert_eq!(only.meta.name, "from-override");
        assert_eq!(only.source_kind, PromptSource::Override);
        assert_eq!(only.source_path, override_path);
    }

    #[test]
    fn invalid_files_are_collected_and_valid_files_still_returned() {
        let root = TempDir::new().unwrap();
        let dir = project_dir(root.path());
        write_prompt(&dir, "good.md", "good", "ok");
        // Missing frontmatter entirely.
        let bad = write_raw(&dir, "bad.md", "no fence here\n");

        let res = discover_prompts(DiscoveryOptions {
            project_root: root.path().to_path_buf(),
            home_dir: None,
            override_dir: None,
        });

        assert_eq!(res.prompts.len(), 1);
        assert_eq!(res.prompts[0].meta.name, "good");
        assert_eq!(res.errors.len(), 1);
        assert_eq!(res.errors[0].0, bad);
        assert!(matches!(
            res.errors[0].1,
            PromptParseError::MissingFrontmatter { .. }
        ));
    }

    #[test]
    fn missing_directories_yield_empty_result() {
        let root = TempDir::new().unwrap();
        // Neither project nor global prompts dirs exist.
        let res = discover_prompts(DiscoveryOptions {
            project_root: root.path().to_path_buf(),
            home_dir: Some(root.path().join("nonexistent-home")),
            override_dir: None,
        });
        assert!(res.prompts.is_empty());
        assert!(res.errors.is_empty());
    }

    #[test]
    fn missing_override_dir_yields_empty_result() {
        let root = TempDir::new().unwrap();
        // Project would have prompts, but override is set to a nonexistent dir,
        // which by the precedence rule still suppresses the project source.
        write_prompt(&project_dir(root.path()), "p.md", "from-project", "x");

        let res = discover_prompts(DiscoveryOptions {
            project_root: root.path().to_path_buf(),
            home_dir: None,
            override_dir: Some(root.path().join("nope")),
        });
        assert!(res.prompts.is_empty());
        assert!(res.errors.is_empty());
    }

    #[test]
    fn non_md_files_are_ignored() {
        let root = TempDir::new().unwrap();
        let dir = project_dir(root.path());
        write_prompt(&dir, "keep.md", "keep", "ok");
        write_raw(&dir, "README.txt", "ignored");
        write_raw(&dir, "notes", "ignored");
        // Subdirectory with a prompt should also be ignored (no recursion).
        let nested = dir.join("nested");
        write_prompt(&nested, "deep.md", "deep", "ok");

        let res = discover_prompts(DiscoveryOptions {
            project_root: root.path().to_path_buf(),
            home_dir: None,
            override_dir: None,
        });
        let names: Vec<&str> = res.prompts.iter().map(|p| p.meta.name.as_str()).collect();
        assert_eq!(names, vec!["keep"]);
    }

    #[test]
    fn discovery_is_deterministic_across_runs() {
        let root = TempDir::new().unwrap();
        let dir = project_dir(root.path());
        // Write in a non-sorted order to make sure ordering comes from us, not
        // filesystem iteration order.
        write_prompt(&dir, "zeta.md", "zeta", "z");
        write_prompt(&dir, "alpha.md", "alpha", "a");
        write_prompt(&dir, "mike.md", "mike", "m");
        write_prompt(&dir, "bravo.md", "bravo", "b");

        let opts = || DiscoveryOptions {
            project_root: root.path().to_path_buf(),
            home_dir: None,
            override_dir: None,
        };
        let first = discover_prompts(opts());
        let second = discover_prompts(opts());
        let names_a: Vec<&str> = first.prompts.iter().map(|p| p.meta.name.as_str()).collect();
        let names_b: Vec<&str> = second
            .prompts
            .iter()
            .map(|p| p.meta.name.as_str())
            .collect();
        assert_eq!(names_a, vec!["alpha", "bravo", "mike", "zeta"]);
        assert_eq!(names_a, names_b);
    }

    #[test]
    fn within_directory_first_filename_wins_on_duplicate_name() {
        let root = TempDir::new().unwrap();
        let dir = project_dir(root.path());
        // Both files declare the same prompt name; alphabetically first wins.
        let first = write_prompt(&dir, "01-first.md", "dup", "first body");
        write_prompt(&dir, "02-second.md", "dup", "second body");

        let res = discover_prompts(DiscoveryOptions {
            project_root: root.path().to_path_buf(),
            home_dir: None,
            override_dir: None,
        });
        assert!(res.errors.is_empty());
        assert_eq!(res.prompts.len(), 1);
        assert_eq!(res.prompts[0].source_path, first);
        assert!(res.prompts[0].body.contains("first body"));
    }

    #[test]
    fn resolve_home_prompts_dir_uses_home_env() {
        // We cannot safely flip $HOME for the whole process, but we can at
        // least exercise the path-construction shape when it's set.
        if let Some(home) = std::env::var_os("HOME") {
            let resolved = resolve_home_prompts_dir().expect("HOME was set");
            assert_eq!(resolved, PathBuf::from(home).join(".pitboss/prompts"));
        }
    }
}
