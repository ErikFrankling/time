//! Commits in the local clones: the only place private and client
//! repositories show up, and the only place diffs exist.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use time_core::event::{Commit, Envelope, Event};

use crate::usage::Activity;
use crate::{stable_id, SERVICE};

/// Lines in one file, in one commit, past which it is machine output: a data
/// dump refresh is a real change and a fake day's work.
const GENERATED_FILE_LINES: usize = 10_000;

/// Lines in one commit past which it is an import or a mass move (a vendored
/// tree, a thousand generated pages): a commit, but no lines of work.
pub const BULK_COMMIT_LINES: u32 = 50_000;

/// Folders whose files are someone else's output: dependencies, build output,
/// generated code. Matched anywhere in the path.
const GENERATED_DIRS: [&str; 9] = [
    "node_modules/",
    "vendor/",
    "third_party/",
    "dist/",
    "build/",
    "target/",
    "generated/",
    "__generated__/",
    "gen/",
];

/// File endings that are lockfiles, minified, generated or binary-ish data,
/// even when small.
const GENERATED_ENDINGS: [&str; 24] = [
    ".lock",
    ".lockb",
    "-lock.json",
    "-lock.yaml",
    "go.sum",
    ".min.js",
    ".min.css",
    ".map",
    ".snap",
    ".pb.go",
    "_pb2.py",
    ".g.dart",
    ".freezed.dart",
    ".gen.ts",
    ".generated.ts",
    ".generated.rs",
    "_generated.go",
    ".svg",
    ".ipynb",
    ".lottie",
    ".log",
    ".csv",
    ".tsv",
    ".jsonl",
];

/// Who to credit, from `Co-Authored-By` trailers: an agent's name, in the
/// form the transcripts use ("Claude Opus 5.5" → "claude-opus-5-5").
fn trailer_agent(message: &str) -> Option<String> {
    const AGENTS: [&str; 12] = [
        "claude",
        "anthropic",
        "codex",
        "openai",
        "gpt",
        "cursor",
        "grok",
        "opencode",
        "deepseek",
        "copilot",
        "gemini",
        "glm",
    ];
    message.lines().find_map(|l| {
        let (key, value) = l.split_once(':')?;
        if !key.trim().eq_ignore_ascii_case("co-authored-by") {
            return None;
        }
        let v = value.trim().to_lowercase();
        let name = v.split('<').next().unwrap_or("").trim();
        let mail = v.split('<').nth(1).unwrap_or("");
        AGENTS
            .iter()
            .any(|a| name.contains(a) || mail.contains(a))
            .then(|| {
                let n: String = name
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                    .collect();
                n.split('-')
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("-")
            })
            .filter(|n| !n.is_empty())
    })
}

/// The agent behind a commit at `at` (ms) in `repo`, and how that is known.
pub fn agent_of(
    message: &str,
    repo: &str,
    at: i64,
    activity: &Activity,
) -> (Option<String>, Option<String>) {
    if let Some(a) = trailer_agent(message) {
        return (Some(a), Some("trailer".into()));
    }
    match activity.agent_at(repo, at) {
        Some(m) => (Some(m.to_owned()), Some("session".into())),
        None => (None, None),
    }
}

/// A path whose lines are not written by hand.
pub fn is_generated(path: &str, lines: usize) -> bool {
    let p = path.to_lowercase();
    let in_dir = |d: &str| p.starts_with(d) || p.contains(&format!("/{d}"));
    lines > GENERATED_FILE_LINES
        || GENERATED_DIRS.iter().any(|d| in_dir(d))
        || GENERATED_ENDINGS.iter().any(|e| p.ends_with(e))
}

/// Find repositories under `root`, without descending into them.
pub fn find_repos(root: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    if root.join(".git").exists() {
        out.push(root.to_path_buf());
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() || p.is_symlink() {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || matches!(name.as_str(), "node_modules" | "target" | "vendor") {
            continue;
        }
        find_repos(&p, depth - 1, out);
    }
}

/// The name a repository is reported under: from the origin remote when
/// there is one, so three clones of a project are one project.
pub(crate) fn repo_name(repo: &git2::Repository, path: &Path) -> String {
    repo.find_remote("origin")
        .ok()
        .and_then(|r| r.url().ok().map(str::to_string))
        .map(|url| {
            url.trim_end_matches('/')
                .trim_end_matches(".git")
                .rsplit(['/', ':'])
                .next()
                .unwrap_or(&url)
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        })
}

/// (added, removed, files) for one commit, generated files left out.
fn commit_lines(diff: &git2::Diff) -> (u32, u32, u32) {
    let mut per_file: HashMap<String, (usize, usize)> = HashMap::new();
    let _ = diff.foreach(
        &mut |_, _| true,
        None,
        None,
        Some(&mut |d, _, line| {
            let path = d
                .new_file()
                .path()
                .or_else(|| d.old_file().path())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let e = per_file.entry(path).or_default();
            match line.origin() {
                '+' => e.0 += 1,
                '-' => e.1 += 1,
                _ => {}
            }
            true
        }),
    );
    let (mut added, mut removed, mut files) = (0, 0, 0);
    for (path, (a, r)) in per_file {
        if is_generated(&path, a + r) {
            continue;
        }
        added += a as u32;
        removed += r as u32;
        files += 1;
    }
    if added + removed > BULK_COMMIT_LINES {
        return (0, 0, files);
    }
    (added, removed, files)
}

/// What identifies a commit across clones, rebases and GitHub: its
/// repository, author time and e-mail, and subject line.
pub fn identity(repo: &str, author_secs: i64, email: &str, subject: &str) -> String {
    format!(
        "commit:{repo}:{author_secs}:{}:{}",
        email.to_lowercase(),
        subject.lines().next().unwrap_or("").trim()
    )
}

fn is_mine(sig: &git2::Signature, authors: &[String]) -> bool {
    let name = sig.name().unwrap_or("").to_lowercase();
    let email = sig.email().unwrap_or("").to_lowercase();
    authors
        .iter()
        .any(|a| email.contains(a.as_str()) || name.contains(a.as_str()))
}

/// Erik's non-merge commits since `since` (Unix seconds), one event each.
pub fn scan(
    repo_path: &Path,
    device: &str,
    authors: &[String],
    since: i64,
    seen: &mut HashSet<String>,
    activity: &Activity,
) -> Result<Vec<Envelope>> {
    let repo = git2::Repository::open(repo_path)?;
    let name = repo_name(&repo, repo_path);
    let mut authors: Vec<String> = authors.iter().map(|a| a.to_lowercase()).collect();
    if authors.is_empty() {
        if let Ok(email) = repo.config().and_then(|c| c.get_string("user.email")) {
            authors.push(email.to_lowercase());
        }
    }
    if authors.is_empty() {
        return Ok(Vec::new());
    }
    let mut walk = repo.revwalk()?;
    // Every local branch: a day on a feature branch is still a day's work.
    walk.push_glob("refs/heads/*")?;
    walk.set_sorting(git2::Sort::TIME)?;
    let mut out = Vec::new();
    let mut stale = 0;
    for oid in walk.flatten() {
        if stale > 500 {
            break;
        }
        let Ok(commit) = repo.find_commit(oid) else {
            continue;
        };
        if commit.time().seconds() < since {
            stale += 1;
            continue;
        }
        stale = 0;
        let sha = oid.to_string();
        // The same commit in two clones, or rebased, is one piece of work; a
        // merge introduces no lines of its own.
        let author = commit.author();
        let key = identity(
            &name,
            author.when().seconds(),
            &String::from_utf8_lossy(author.email_bytes()),
            &String::from_utf8_lossy(commit.summary_bytes().unwrap_or_default()),
        );
        if !seen.insert(sha.clone())
            || commit.parent_count() > 1
            || !is_mine(&author, &authors)
            || !seen.insert(key.clone())
        {
            continue;
        }
        let diff = commit.tree().and_then(|tree| {
            let parent = commit.parent(0).ok().and_then(|p| p.tree().ok());
            // Blobs past 1 MB count as binary: never loaded, never lines.
            // A data dump in one repository used to take gigabytes here.
            let mut opts = git2::DiffOptions::new();
            opts.max_size(1 << 20);
            let mut diff = repo.diff_tree_to_tree(parent.as_ref(), Some(&tree), Some(&mut opts))?;
            // A moved file is a rename, not its every line removed and added.
            let mut find = git2::DiffFindOptions::new();
            find.renames(true).rename_limit(2_000);
            diff.find_similar(Some(&mut find))?;
            Ok(diff)
        });
        let (added, removed, files) = diff.map(|d| commit_lines(&d)).unwrap_or_default();
        // When it was written: a rebase keeps the author's time.
        let at = author.when().seconds() * 1000;
        let (agent, agent_evidence) = agent_of(commit.message().unwrap_or(""), &name, at, activity);
        out.push(Envelope {
            id: stable_id(&key),
            device: device.to_owned(),
            service: SERVICE.to_owned(),
            start_ms: at,
            end_ms: at,
            event: Event::Commit(Commit {
                repo: name.clone(),
                sha,
                path: Some(repo_path.display().to_string()),
                lines_added: added,
                lines_removed: removed,
                files,
                agent,
                agent_evidence,
                source: Some("local".into()),
            }),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(repo: &git2::Repository, file: &str, content: &str, email: &str, msg: &str) {
        let path = repo.workdir().unwrap().join(file);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(file)).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("Someone", email).unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)
            .unwrap();
    }

    #[test]
    fn counts_erik_s_commits_once_and_skips_generated_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects/husk");
        std::fs::create_dir_all(&root).unwrap();
        let repo = git2::Repository::init(&root).unwrap();
        commit(
            &repo,
            "src/a.rs",
            "a\nb\nc\n",
            "erik@example.com",
            "three lines",
        );
        commit(
            &repo,
            "Cargo.lock",
            &"x\n".repeat(500),
            "erik@example.com",
            "lockfile only",
        );
        commit(&repo, "src/b.rs", "q\n", "someone@else.com", "not mine");

        let mut repos = Vec::new();
        find_repos(&dir.path().join("projects"), 4, &mut repos);
        assert_eq!(repos, vec![root.clone()]);

        let mut seen = HashSet::new();
        let events = scan(
            &root,
            "pc",
            &["erik@".into()],
            0,
            &mut seen,
            &Activity::default(),
        )
        .unwrap();
        assert_eq!(
            events.len(),
            2,
            "two of Erik's commits, the stranger's left out"
        );
        let lines: Vec<(u32, u32)> = events
            .iter()
            .map(|e| match &e.event {
                Event::Commit(c) => (c.lines_added, c.files),
                _ => unreachable!(),
            })
            .collect();
        assert!(lines.contains(&(3, 1)));
        assert!(
            lines.contains(&(0, 0)),
            "the lockfile counts as a commit, not as lines"
        );
        // A second clone, or a second run, adds nothing.
        assert!(scan(
            &root,
            "pc",
            &["erik@".into()],
            0,
            &mut seen,
            &Activity::default()
        )
        .unwrap()
        .is_empty());
        let again = scan(
            &root,
            "pc",
            &["erik@".into()],
            0,
            &mut HashSet::new(),
            &Activity::default(),
        )
        .unwrap();
        assert_eq!(
            again.iter().map(|e| &e.id).collect::<Vec<_>>(),
            events.iter().map(|e| &e.id).collect::<Vec<_>>(),
            "ids are stable across runs"
        );
    }
}
