use crate::persistence::{GitRepository, TemporaryIndex, validate_path};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

fn checked_path(root: &Path, relative: &str) -> Result<PathBuf> {
    validate_path(relative)?;
    let mut path = root.to_path_buf();
    for part in relative.split('/') {
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink(),
                "capture cannot traverse a symlink: {relative}"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(path)
}

pub fn capture(
    repo: &GitRepository,
    worktree: &Path,
    base_tree: &str,
    paths: &[String],
) -> Result<String> {
    ensure!(
        !fs::symlink_metadata(worktree)?.file_type().is_symlink(),
        "worktree root cannot be a symlink"
    );
    let root = fs::canonicalize(worktree)?;
    ensure!(root.is_dir(), "worktree must be a directory");
    let workspace = GitRepository::open(&root)?;
    ensure!(
        repo.common_dir() == workspace.common_dir(),
        "capture workspace belongs to a different repository"
    );
    let top = workspace.run(&["rev-parse", "--show-toplevel"], &[], None)?;
    ensure!(
        fs::canonicalize(std::str::from_utf8(&top)?.trim())? == root,
        "capture must use the worktree root"
    );
    let mut seen = BTreeSet::new();
    let mut reads = Vec::new();
    let mut updates = BTreeMap::new();
    for relative in paths {
        ensure!(seen.insert(relative), "duplicate capture path");
        let path = checked_path(&root, relative)?;
        match fs::metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file(),
                    "capture allowlist requires individual regular files"
                );
                let bytes = fs::read(&path)?;
                let mode = file_mode(&metadata);
                let blob = repo.hash_blob(&bytes)?;
                updates.insert(relative.clone(), Some((mode.clone(), blob)));
                reads.push((relative, Some((bytes, mode))));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                updates.insert(relative.clone(), None);
                reads.push((relative, None));
            }
            Err(e) => return Err(e.into()),
        }
    }
    for (relative, previous) in reads {
        let path = checked_path(&root, relative)?;
        match previous {
            Some((bytes, mode)) => {
                ensure!(
                    fs::read(&path)? == bytes && file_mode(&fs::metadata(&path)?) == mode,
                    "workspace changed during capture; pause writers and retry"
                );
            }
            None => ensure!(
                !path.exists(),
                "workspace changed during capture; pause writers and retry"
            ),
        }
    }
    repo.tree_with_updates(base_tree, &updates)
}

fn file_mode(metadata: &fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 != 0 {
            return "100755".into();
        }
    }
    let _ = metadata;
    "100644".into()
}

pub fn diff(repo: &GitRepository, before: &str, after: &str) -> Result<Vec<u8>> {
    ensure!(
        repo.object_kind(before)? == "tree" && repo.object_kind(after)? == "tree",
        "diff requires source trees"
    );
    repo.run(
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "--full-index",
            "--no-renames",
            before,
            after,
            "--",
        ],
        &[],
        None,
    )
}

pub fn apply_patch(repo: &GitRepository, base_tree: &str, patch: &[u8]) -> Result<String> {
    ensure!(
        repo.object_kind(base_tree)? == "tree",
        "patch base must be a tree"
    );
    if patch.is_empty() {
        return Ok(base_tree.to_owned());
    }
    let index = TemporaryIndex::new()?;
    repo.run(&["read-tree", base_tree], &[], Some(index.path()))?;
    repo.run(
        &["apply", "--cached", "--binary", "--whitespace=nowarn", "-"],
        patch,
        Some(index.path()),
    )
    .context("patch conflicts with the exact source base")?;
    Ok(
        String::from_utf8(repo.run(&["write-tree"], &[], Some(index.path()))?)?
            .trim()
            .to_owned(),
    )
}

pub fn compose(
    repo: &GitRepository,
    base_tree: &str,
    changes: &[(String, String)],
) -> Result<String> {
    let mut composed = base_tree.to_owned();
    for (before, after) in changes {
        composed = apply_patch(repo, &composed, &diff(repo, before, after)?)?;
    }
    Ok(composed)
}

pub fn materialize(repo: &GitRepository, source_tree: &str, destination: &Path) -> Result<String> {
    ensure!(
        destination.is_absolute(),
        "new workspace destination must be absolute"
    );
    ensure!(
        fs::symlink_metadata(destination).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "new workspace destination already exists or cannot be inspected"
    );
    ensure!(
        !destination
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "workspace destination contains parent traversal"
    );
    let parent = fs::canonicalize(
        destination
            .parent()
            .context("workspace destination has no parent")?,
    )?;
    let destination = parent.join(
        destination
            .file_name()
            .context("workspace destination has no name")?,
    );
    repo.flatten_tree(source_tree)?;
    let commit = repo.create_commit(source_tree, None, "Rev Queue restored workspace\n")?;
    let reference = format!("refs/revqueue/workspaces/{commit}");
    repo.run(
        &["update-ref", "--no-deref", &reference, &commit],
        &[],
        None,
    )?;
    let filters = repo
        .command()
        .args([
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            "^filter\\.",
        ])
        .output()?;
    ensure!(
        filters.status.success() || filters.status.code() == Some(1),
        "cannot inspect checkout filters"
    );
    let mut drivers = BTreeSet::new();
    for key in filters
        .stdout
        .split(|b| *b == 0)
        .filter(|key| !key.is_empty())
    {
        if let Some((driver, _)) = std::str::from_utf8(key)?.rsplit_once('.') {
            drivers.insert(driver.to_owned());
        }
    }
    let mut cmd = repo.command();
    for driver in drivers {
        for setting in ["smudge", "process"] {
            cmd.arg("-c").arg(format!("{driver}.{setting}="));
        }
        cmd.arg("-c").arg(format!("{driver}.required=false"));
    }
    cmd.args([
        "-c",
        "core.autocrlf=false",
        "-c",
        "core.hooksPath=/dev/null",
        "worktree",
        "add",
        "--detach",
        "--",
    ])
    .arg(&destination)
    .arg(&commit);
    GitRepository::execute(cmd, &[])?;
    Ok(commit)
}

pub fn contains_change(
    repo: &GitRepository,
    before: &str,
    after: &str,
    candidate: &str,
) -> Result<bool> {
    let reverse = diff(repo, after, before)?;
    let Ok(without) = apply_patch(repo, candidate, &reverse) else {
        return Ok(false);
    };
    Ok(apply_patch(repo, &without, &diff(repo, before, after)?)? == candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::tests::TestRepo;

    #[test]
    fn capture_only_allowlisted_files_and_leave_user_index_untouched() {
        let t = TestRepo::new();
        let r = &t.repo;
        let base = t.tree(&[
            ("kept.rs", b"keep\n"),
            ("changed.rs", b"old\n"),
            ("deleted.rs", b"remove\n"),
        ]);
        fs::write(t.root.join("changed.rs"), b"new\r\n").unwrap();
        fs::write(t.root.join("unrelated.rs"), b"dirty\n").unwrap();
        fs::write(t.root.join("sp ace.rs"), b"space\n").unwrap();
        r.run(&["add", "unrelated.rs"], &[], None).unwrap();
        let index_before = fs::read(r.git_dir().join("index")).unwrap();
        let captured = capture(
            r,
            &t.root,
            &base,
            &["changed.rs".into(), "deleted.rs".into(), "sp ace.rs".into()],
        )
        .unwrap();
        assert_eq!(r.read_file(&captured, "changed.rs").unwrap(), b"new\r\n");
        assert_eq!(r.read_file(&captured, "kept.rs").unwrap(), b"keep\n");
        assert!(r.read_file(&captured, "deleted.rs").is_err());
        assert!(r.read_file(&captured, "unrelated.rs").is_err());
        assert_eq!(r.flatten_tree(&captured).unwrap().len(), 3);
        assert_eq!(fs::read(r.git_dir().join("index")).unwrap(), index_before);
    }

    #[test]
    fn patch_composition_and_presence_detection_are_exact() {
        let t = TestRepo::new();
        let r = &t.repo;
        let base = t.tree(&[("a.rs", b"a\n"), ("b.rs", b"b\n"), ("binary", b"\0\x01")]);
        let one = t.tree(&[
            ("a.rs", b"changed a\n"),
            ("b.rs", b"b\n"),
            ("binary", b"\0\x02"),
        ]);
        let two = t.tree(&[
            ("a.rs", b"a\n"),
            ("b.rs", b"changed b\n"),
            ("binary", b"\0\x01"),
        ]);
        let combined = compose(
            r,
            &base,
            &[(base.clone(), one.clone()), (base.clone(), two.clone())],
        )
        .unwrap();
        assert_eq!(r.read_file(&combined, "a.rs").unwrap(), b"changed a\n");
        assert_eq!(r.read_file(&combined, "b.rs").unwrap(), b"changed b\n");
        assert_eq!(r.read_file(&combined, "binary").unwrap(), b"\0\x02");
        assert!(contains_change(r, &base, &one, &combined).unwrap());
        assert!(!contains_change(r, &base, &one, &base).unwrap());
        assert!(
            compose(
                r,
                &base,
                &[(base.clone(), one.clone()), (base.clone(), one.clone())]
            )
            .is_err()
        );
        assert_eq!(
            apply_patch(r, &one, &diff(r, &one, &base).unwrap()).unwrap(),
            base
        );
        assert!(!r.git_dir().join("index").exists());
    }

    #[cfg(unix)]
    #[test]
    fn capture_rejects_symlink_traversal_and_preserves_executable_modes() {
        use std::os::unix::{fs::PermissionsExt, fs::symlink};
        let t = TestRepo::new();
        let r = &t.repo;
        let base = r.empty_tree().unwrap();
        fs::write(t.root.join("script"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(t.root.join("script"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink(t.root.join("script"), t.root.join("link")).unwrap();
        symlink(&t.root, t.root.join("directory-link")).unwrap();
        assert!(capture(r, &t.root, &base, &["link".into()]).is_err());
        assert!(capture(r, &t.root, &base, &["directory-link/script".into()]).is_err());
        assert!(capture(r, &t.root, &base, &["../outside".into()]).is_err());
        let tree = capture(r, &t.root, &base, &["script".into()]).unwrap();
        assert_eq!(r.flatten_tree(&tree).unwrap()["script"].mode, "100755");
    }

    #[test]
    fn materialization_creates_a_new_detached_workspace_without_touching_source() {
        let t = TestRepo::new();
        let r = &t.repo;
        let tree = t.tree(&[
            ("file.rs", b"checkpoint\n"),
            (".gitattributes", b"*.rs filter=example\n"),
        ]);
        r.run(&["config", "filter.example.smudge", "false"], &[], None)
            .unwrap();
        r.run(&["config", "filter.example.process", "false"], &[], None)
            .unwrap();
        r.run(&["config", "filter.example.required", "true"], &[], None)
            .unwrap();
        fs::write(t.root.join("file.rs"), b"existing dirty work\n").unwrap();
        let destination = t.root.join("restored");
        let commit = materialize(r, &tree, &destination).unwrap();
        assert_eq!(
            fs::read(destination.join("file.rs")).unwrap(),
            b"checkpoint\n"
        );
        assert_eq!(
            fs::read(t.root.join("file.rs")).unwrap(),
            b"existing dirty work\n"
        );
        assert_eq!(r.object_kind(&commit).unwrap(), "commit");
        assert!(materialize(r, &tree, &destination).is_err());
        assert!(r.run(&["symbolic-ref", "HEAD"], &[], None).is_ok());
    }
}
