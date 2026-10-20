use anyhow::{Context, Result, bail, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Debug)]
pub struct GitRepository {
    path: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
}

#[derive(Debug)]
pub struct Conflict;
impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "goal head changed; reload and reconcile")
    }
}
impl std::error::Error for Conflict {}

pub struct TransactionObjects<'a> {
    pub records: &'a BTreeMap<String, Vec<u8>>,
    pub trees: &'a [String],
    pub blobs: &'a [String],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: String,
    pub kind: String,
    pub oid: String,
    pub name: String,
}

pub(crate) struct TemporaryIndex(PathBuf);
impl TemporaryIndex {
    pub(crate) fn new() -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "revqueue-index-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir)?;
        Ok(Self(dir.join("index")))
    }
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TemporaryIndex {
    fn drop(&mut self) {
        if let Some(p) = self.0.parent() {
            let _ = fs::remove_dir_all(p);
        }
    }
}

pub fn validate_oid(oid: &str) -> Result<()> {
    ensure!(
        (oid.len() == 40 || oid.len() == 64) && oid.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid Git object ID"
    );
    Ok(())
}
pub fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && !path.contains(['\0', '\\']) && !path.starts_with('/'),
        "invalid repository path"
    );
    ensure!(
        path.split('/')
            .all(|s| !s.is_empty() && s != "." && s != ".." && !s.eq_ignore_ascii_case(".git")),
        "unsafe repository path"
    );
    Ok(())
}
fn goal_ref(goal: &str) -> Result<String> {
    ensure!(
        !goal.is_empty() && goal.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "invalid goal ID"
    );
    Ok(format!("refs/revqueue/goals/{goal}"))
}

impl GitRepository {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = fs::canonicalize(path)?;
        let output = Self::base_command(&path)
            .args(["rev-parse", "--absolute-git-dir"])
            .output()?;
        ensure!(output.status.success(), "not a Git repository");
        let git_dir = fs::canonicalize(PathBuf::from(
            String::from_utf8(output.stdout)?.trim_end_matches('\n'),
        ))?;
        let common = Self::base_command(&path)
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .output()?;
        ensure!(
            common.status.success(),
            "cannot locate common Git directory"
        );
        let common_dir = fs::canonicalize(PathBuf::from(
            String::from_utf8(common.stdout)?.trim_end_matches('\n'),
        ))?;
        Ok(Self {
            path,
            git_dir,
            common_dir,
        })
    }
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        fs::create_dir_all(path.as_ref())?;
        let output = Self::base_command(path.as_ref())
            .args(["init", "--quiet"])
            .output()?;
        ensure!(
            output.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self::open(path)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }
    pub(crate) fn command(&self) -> Command {
        Self::base_command(&self.path)
    }
    fn base_command(path: &Path) -> Command {
        let mut c = Command::new("git");
        c.arg("-C").arg(path).args([
            "--no-replace-objects",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
        ]);
        for name in [
            "GIT_DIR",
            "GIT_COMMON_DIR",
            "GIT_SHALLOW_FILE",
            "GIT_GRAFT_FILE",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_EXTERNAL_DIFF",
            "GIT_NAMESPACE",
        ] {
            c.env_remove(name);
        }
        c.env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        c
    }
    pub(crate) fn run(&self, args: &[&str], input: &[u8], index: Option<&Path>) -> Result<Vec<u8>> {
        let mut cmd = self.command();
        cmd.args(args);
        if let Some(index) = index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        Self::execute(cmd, input)
    }
    pub(crate) fn execute(mut cmd: Command, input: &[u8]) -> Result<Vec<u8>> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("launch Git")?;
        let mut stdin = child.stdin.take().context("Git stdin unavailable")?;
        // Feed stdin concurrently so a large patch cannot deadlock on diagnostic output.
        let bytes = input.to_vec();
        let writer = std::thread::spawn(move || stdin.write_all(&bytes));
        let output = child.wait_with_output()?;
        let written = writer
            .join()
            .map_err(|_| anyhow::anyhow!("Git stdin writer panicked"))?;
        ensure!(
            output.status.success(),
            "Git operation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        written?;
        Ok(output.stdout)
    }
    pub fn object_kind(&self, oid: &str) -> Result<String> {
        validate_oid(oid)?;
        Ok(
            String::from_utf8(self.run(&["cat-file", "-t", oid], &[], None)?)?
                .trim()
                .to_owned(),
        )
    }
    pub fn object_size(&self, oid: &str) -> Result<u64> {
        validate_oid(oid)?;
        Ok(
            std::str::from_utf8(&self.run(&["cat-file", "-s", oid], &[], None)?)?
                .trim()
                .parse()?,
        )
    }
    pub fn hash_blob(&self, bytes: &[u8]) -> Result<String> {
        Ok(
            String::from_utf8(self.run(&["hash-object", "-w", "--stdin"], bytes, None)?)?
                .trim()
                .to_owned(),
        )
    }
    pub fn read_blob(&self, oid: &str) -> Result<Vec<u8>> {
        validate_oid(oid)?;
        self.run(&["cat-file", "blob", oid], &[], None)
    }
    pub fn empty_tree(&self) -> Result<String> {
        self.write_tree(&[])
    }
    pub fn tree_entries(&self, tree: &str) -> Result<Vec<TreeEntry>> {
        validate_oid(tree)?;
        let output = self.run(&["ls-tree", "-z", tree], &[], None)?;
        output
            .split(|b| *b == 0)
            .filter(|v| !v.is_empty())
            .map(|row| {
                let (meta, name) = row.split_at(
                    row.iter()
                        .position(|b| *b == b'\t')
                        .context("malformed tree entry")?,
                );
                let fields: Vec<&str> = std::str::from_utf8(meta)?.split(' ').collect();
                ensure!(fields.len() == 3, "malformed tree metadata");
                let name = std::str::from_utf8(&name[1..])
                    .context("non-UTF-8 filename is unsupported")?
                    .to_owned();
                Ok(TreeEntry {
                    mode: fields[0].to_owned(),
                    kind: fields[1].to_owned(),
                    oid: fields[2].to_owned(),
                    name,
                })
            })
            .collect()
    }
    pub fn flatten_tree(&self, tree: &str) -> Result<BTreeMap<String, TreeEntry>> {
        fn walk(
            repo: &GitRepository,
            oid: &str,
            prefix: &str,
            out: &mut BTreeMap<String, TreeEntry>,
        ) -> Result<()> {
            for entry in repo.tree_entries(oid)? {
                let path = format!("{prefix}{}", entry.name);
                validate_path(&path)?;
                if entry.kind == "tree" {
                    walk(repo, &entry.oid, &format!("{path}/"), out)?;
                } else {
                    ensure!(
                        entry.kind == "blob",
                        "submodules are unsupported in v1 source snapshots"
                    );
                    out.insert(path, entry);
                }
            }
            Ok(())
        }
        ensure!(self.object_kind(tree)? == "tree", "source must be a tree");
        let mut files = BTreeMap::new();
        walk(self, tree, "", &mut files)?;
        Ok(files)
    }
    pub fn write_tree(&self, entries: &[TreeEntry]) -> Result<String> {
        let mut input = Vec::new();
        let mut names = std::collections::BTreeSet::new();
        for e in entries {
            validate_oid(&e.oid)?;
            ensure!(
                !e.name.contains('/')
                    && !e.name.contains('\0')
                    && !e.name.is_empty()
                    && names.insert(&e.name),
                "invalid or duplicate tree name"
            );
            ensure!(
                (e.mode == "040000" && e.kind == "tree")
                    || (["100644", "100755", "120000"].contains(&e.mode.as_str())
                        && e.kind == "blob"),
                "unsupported tree entry mode"
            );
            input.extend_from_slice(
                format!("{} {} {}\t{}\0", e.mode, e.kind, e.oid, e.name).as_bytes(),
            );
        }
        Ok(
            String::from_utf8(self.run(&["mktree", "-z"], &input, None)?)?
                .trim()
                .to_owned(),
        )
    }
    pub fn tree_with_updates(
        &self,
        base: &str,
        updates: &BTreeMap<String, Option<(String, String)>>,
    ) -> Result<String> {
        ensure!(self.object_kind(base)? == "tree", "base must be a tree");
        let index = TemporaryIndex::new()?;
        self.run(&["read-tree", base], &[], Some(index.path()))?;
        let mut input = Vec::new();
        let zero = "0".repeat(base.len());
        for (path, entry) in updates {
            validate_path(path)?;
            match entry {
                Some((mode, oid)) => {
                    ensure!(
                        ["100644", "100755", "120000"].contains(&mode.as_str()),
                        "unsupported file mode"
                    );
                    ensure!(
                        self.object_kind(oid)? == "blob",
                        "file must reference a blob"
                    );
                    input.extend_from_slice(format!("{mode} {oid}\t{path}\0").as_bytes());
                }
                None => input.extend_from_slice(format!("0 {zero}\t{path}\0").as_bytes()),
            }
        }
        self.run(
            &["update-index", "-z", "--index-info"],
            &input,
            Some(index.path()),
        )?;
        Ok(
            String::from_utf8(self.run(&["write-tree"], &[], Some(index.path()))?)?
                .trim()
                .to_owned(),
        )
    }
    pub fn read_file(&self, treeish: &str, path: &str) -> Result<Vec<u8>> {
        validate_oid(treeish)?;
        validate_path(path)?;
        self.run(&["show", &format!("{treeish}:{path}")], &[], None)
    }
    pub fn read_json<T: DeserializeOwned>(&self, commit: &str, path: &str) -> Result<T> {
        Ok(serde_json::from_slice(&self.read_file(commit, path)?)?)
    }
    pub fn create_commit(&self, tree: &str, parent: Option<&str>, message: &str) -> Result<String> {
        ensure!(
            self.object_kind(tree)? == "tree",
            "commit root must be a tree"
        );
        let mut cmd = self.command();
        cmd.args(["commit-tree", tree]);
        if let Some(p) = parent {
            ensure!(self.object_kind(p)? == "commit", "parent must be a commit");
            cmd.args(["-p", p]);
        }
        cmd.env("GIT_AUTHOR_NAME", "Rev Queue")
            .env("GIT_AUTHOR_EMAIL", "revqueue@localhost")
            .env("GIT_COMMITTER_NAME", "Rev Queue")
            .env("GIT_COMMITTER_EMAIL", "revqueue@localhost");
        Ok(String::from_utf8(Self::execute(cmd, message.as_bytes())?)?
            .trim()
            .to_owned())
    }
    pub fn read_ref(&self, goal: &str) -> Result<Option<String>> {
        let name = goal_ref(goal)?;
        let output = self
            .command()
            .args(["rev-parse", "--verify", "--quiet", &name])
            .output()?;
        if output.status.success() {
            return Ok(Some(String::from_utf8(output.stdout)?.trim().to_owned()));
        }
        if output.status.code() == Some(1) {
            return Ok(None);
        }
        bail!(
            "cannot read goal ref: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    }
    pub fn list_goals(&self) -> Result<Vec<(String, String)>> {
        let out = self.run(
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/revqueue/goals/",
            ],
            &[],
            None,
        )?;
        String::from_utf8(out)?
            .lines()
            .map(|l| {
                let (r, o) = l.split_once(' ').context("invalid ref listing")?;
                Ok((
                    r.trim_start_matches("refs/revqueue/goals/").to_owned(),
                    o.to_owned(),
                ))
            })
            .collect()
    }
    fn application_refs(&self) -> Result<BTreeMap<String, String>> {
        let output = self.run(
            &[
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                "refs/revqueue/",
            ],
            &[],
            None,
        )?;
        Self::parse_bundle_refs(&output)
    }
    fn parse_bundle_refs(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
        let mut refs = BTreeMap::new();
        for line in std::str::from_utf8(bytes)?.lines() {
            let (oid, reference) = line.split_once(' ').context("invalid bundle ref record")?;
            validate_oid(oid)?;
            let valid = if let Some(goal) = reference.strip_prefix("refs/revqueue/goals/") {
                goal_ref(goal)? == reference
            } else if let Some(workspace) = reference.strip_prefix("refs/revqueue/workspaces/") {
                validate_oid(workspace).is_ok()
            } else {
                false
            };
            ensure!(valid, "bundle includes an unsupported ref: {reference}");
            ensure!(
                refs.insert(reference.to_owned(), oid.to_owned()).is_none(),
                "duplicate bundle ref: {reference}"
            );
        }
        Ok(refs)
    }
    pub fn export_bundle(&self, destination: &Path) -> Result<()> {
        let refs = self.application_refs()?;
        ensure!(!refs.is_empty(), "no application refs to export");
        let destination = if destination.is_absolute() {
            destination.to_path_buf()
        } else {
            std::env::current_dir()?.join(destination)
        };
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .context("backup destination must be a new file")?;
        let mut cmd = self.command();
        cmd.args(["bundle", "create"]).arg(&destination);
        for reference in refs.keys() {
            cmd.arg(reference);
        }
        if let Err(error) = Self::execute(cmd, &[]) {
            let _ = fs::remove_file(destination);
            return Err(error);
        }
        Ok(())
    }
    pub fn import_bundle(&self, source: &Path) -> Result<()> {
        let source = fs::canonicalize(source)?;
        ensure!(source.is_file(), "backup source must be a file");
        let mut list = self.command();
        list.args(["bundle", "list-heads"]).arg(&source);
        let refs = Self::parse_bundle_refs(&Self::execute(list, &[])?)?;
        ensure!(!refs.is_empty(), "bundle contains no application refs");
        let mut verify = self.command();
        verify.args(["bundle", "verify"]).arg(&source);
        Self::execute(verify, &[])?;
        let mut import = self.command();
        import.args(["bundle", "unbundle"]).arg(&source);
        ensure!(
            Self::parse_bundle_refs(&Self::execute(import, &[])?)? == refs,
            "bundle changed while importing"
        );
        let mut transaction = String::from("start\n");
        for (reference, oid) in refs {
            ensure!(
                self.object_kind(&oid)? == "commit",
                "application refs must point to commits"
            );
            let symbolic = self
                .command()
                .args(["symbolic-ref", "--quiet", &reference])
                .output()?;
            ensure!(
                symbolic.status.code() == Some(1),
                "application ref is symbolic or cannot be inspected: {reference}"
            );
            let existing = self
                .command()
                .args(["rev-parse", "--verify", "--quiet", &reference])
                .output()?;
            if existing.status.success() {
                let previous = String::from_utf8(existing.stdout)?.trim().to_owned();
                ensure!(
                    previous == oid,
                    "import would overwrite existing application history: {reference}"
                );
                transaction.push_str(&format!("verify {reference} {previous}\n"));
            } else {
                ensure!(
                    existing.status.code() == Some(1),
                    "cannot inspect application ref: {reference}"
                );
                transaction.push_str(&format!("create {reference} {oid}\n"));
            }
        }
        transaction.push_str("prepare\ncommit\n");
        self.run(
            &["update-ref", "--no-deref", "--stdin"],
            transaction.as_bytes(),
            None,
        )?;
        Ok(())
    }
    pub fn history(&self, head: &str) -> Result<Vec<String>> {
        validate_oid(head)?;
        Ok(String::from_utf8(self.run(
            &["rev-list", "--reverse", "--first-parent", head],
            &[],
            None,
        )?)?
        .lines()
        .map(str::to_owned)
        .collect())
    }
    pub fn write_transaction<T: Serialize, S: Serialize>(
        &self,
        goal: &str,
        expected: Option<&str>,
        transaction: &T,
        state: &S,
        objects: TransactionObjects<'_>,
    ) -> Result<String> {
        let TransactionObjects {
            records,
            trees,
            blobs,
        } = objects;
        let reference = goal_ref(goal)?;
        if let Some(oid) = expected {
            validate_oid(oid)?;
        }
        let mut entries = match expected {
            Some(head) => self.tree_entries(head)?,
            None => vec![],
        };
        let mut updates = records.clone();
        for path in updates.keys() {
            validate_path(path)?;
            ensure!(
                path.starts_with("objects/"),
                "records must live under objects/"
            );
        }
        updates.insert("format.json".into(), serde_json::to_vec(&serde_json::json!({"schema_version":1,"object_format":if self.empty_tree()?.len()==40 {"sha1"} else {"sha256"}}))?);
        updates.insert("transaction.json".into(), serde_json::to_vec(transaction)?);
        updates.insert("state.json".into(), serde_json::to_vec(state)?);
        let old_tree = self.write_tree(&entries)?;
        let mut file_updates = BTreeMap::new();
        for (path, bytes) in updates {
            if path.starts_with("objects/")
                && let Some(head) = expected
                && let Ok(old) = self.read_file(head, &path)
            {
                ensure!(old == bytes, "immutable record cannot be replaced: {path}");
            }
            file_updates.insert(path, Some(("100644".into(), self.hash_blob(&bytes)?)));
        }
        let metadata_tree = self.tree_with_updates(&old_tree, &file_updates)?;
        entries = self.tree_entries(&metadata_tree)?;
        for (folder, oids, kind, mode) in [
            ("snapshots", trees, "tree", "040000"),
            ("artifacts", blobs, "blob", "100644"),
        ] {
            let mut retained = if let Some(e) = entries.iter().find(|e| e.name == folder) {
                self.tree_entries(&e.oid)?
            } else {
                vec![]
            };
            for oid in oids {
                ensure!(
                    self.object_kind(oid)? == kind,
                    "invalid retained object kind"
                );
                if !retained.iter().any(|e| e.name == *oid) {
                    retained.push(TreeEntry {
                        name: oid.clone(),
                        oid: oid.clone(),
                        kind: kind.into(),
                        mode: mode.into(),
                    });
                }
            }
            entries.retain(|e| e.name != folder);
            entries.push(TreeEntry {
                mode: "040000".into(),
                kind: "tree".into(),
                oid: self.write_tree(&retained)?,
                name: folder.into(),
            });
        }
        let tree = self.write_tree(&entries)?;
        let head = self.create_commit(&tree, expected, "Rev Queue transaction\n")?;
        let zero = "0".repeat(head.len());
        if self
            .run(
                &[
                    "update-ref",
                    "--no-deref",
                    &reference,
                    &head,
                    expected.unwrap_or(&zero),
                ],
                &[],
                None,
            )
            .is_err()
        {
            if self.read_ref(goal)?.as_deref() != expected {
                return Err(Conflict.into());
            }
            bail!("failed to advance goal ref");
        }
        Ok(head)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) struct TestRepo {
        pub repo: GitRepository,
        pub root: PathBuf,
    }
    impl TestRepo {
        pub fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "revqueue-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self {
                repo: GitRepository::init(&root).unwrap(),
                root,
            }
        }
        pub fn tree(&self, files: &[(&str, &[u8])]) -> String {
            let updates = files
                .iter()
                .map(|(p, b)| {
                    (
                        (*p).to_owned(),
                        Some(("100644".into(), self.repo.hash_blob(b).unwrap())),
                    )
                })
                .collect();
            self.repo
                .tree_with_updates(&self.repo.empty_tree().unwrap(), &updates)
                .unwrap()
        }
    }
    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn transactions_are_cas_immutable_and_replayable() {
        let t = TestRepo::new();
        let r = &t.repo;
        assert_eq!(r.read_ref("goal-1").unwrap(), None);
        let records = BTreeMap::from([(
            "objects/thunks/one.json".into(),
            br#"{"title":"first"}"#.to_vec(),
        )]);
        let state = serde_json::json!({"sequence":1});
        let first = r
            .write_transaction(
                "goal-1",
                None,
                &vec!["created"],
                &state,
                TransactionObjects {
                    records: &records,
                    trees: &[],
                    blobs: &[],
                },
            )
            .unwrap();
        let error = r
            .write_transaction(
                "goal-1",
                None,
                &vec!["created"],
                &state,
                TransactionObjects {
                    records: &records,
                    trees: &[],
                    blobs: &[],
                },
            )
            .unwrap_err();
        assert!(error.downcast_ref::<Conflict>().is_some());
        let second = r
            .write_transaction(
                "goal-1",
                Some(&first),
                &vec!["submitted"],
                &serde_json::json!({"sequence":2}),
                TransactionObjects {
                    records: &BTreeMap::new(),
                    trees: &[],
                    blobs: &[],
                },
            )
            .unwrap();
        assert_eq!(
            r.history(&second).unwrap(),
            vec![first.clone(), second.clone()]
        );
        assert_eq!(
            r.read_json::<Vec<String>>(&first, "transaction.json")
                .unwrap(),
            vec!["created"]
        );
        assert_eq!(
            r.read_json::<Vec<String>>(&second, "transaction.json")
                .unwrap(),
            vec!["submitted"]
        );
        assert_eq!(
            r.read_file(&second, "objects/thunks/one.json").unwrap(),
            records["objects/thunks/one.json"]
        );
        let bad = BTreeMap::from([("objects/thunks/one.json".into(), b"different".to_vec())]);
        assert!(
            r.write_transaction(
                "goal-1",
                Some(&second),
                &(),
                &(),
                TransactionObjects {
                    records: &bad,
                    trees: &[],
                    blobs: &[]
                }
            )
            .is_err()
        );
        assert_eq!(r.read_ref("goal-1").unwrap(), Some(second.clone()));
        assert_eq!(
            GitRepository::open(&t.root).unwrap().list_goals().unwrap(),
            vec![("goal-1".into(), second)]
        );
        assert!(
            r.run(&["rev-parse", "--verify", "HEAD"], &[], None)
                .is_err()
        );
        assert!(!r.git_dir().join("index").exists());
    }

    #[test]
    fn retained_source_and_artifacts_survive_gc() {
        let t = TestRepo::new();
        let r = &t.repo;
        let tree = t.tree(&[("nested/source.rs", b"fn main() {}\n")]);
        let blob = r.hash_blob(b"test output").unwrap();
        let head = r
            .write_transaction(
                "gc",
                None,
                &(),
                &(),
                TransactionObjects {
                    records: &BTreeMap::new(),
                    trees: std::slice::from_ref(&tree),
                    blobs: std::slice::from_ref(&blob),
                },
            )
            .unwrap();
        r.run(&["reflog", "expire", "--expire=now", "--all"], &[], None)
            .unwrap();
        r.run(&["gc", "--prune=now"], &[], None).unwrap();
        assert_eq!(
            r.read_file(&tree, "nested/source.rs").unwrap(),
            b"fn main() {}\n"
        );
        assert_eq!(r.read_blob(&blob).unwrap(), b"test output");
        assert_eq!(r.object_size(&blob).unwrap(), 11);
        let second = r
            .write_transaction(
                "gc",
                Some(&head),
                &(),
                &(),
                TransactionObjects {
                    records: &BTreeMap::new(),
                    trees: &[],
                    blobs: &[],
                },
            )
            .unwrap();
        assert_eq!(
            r.read_file(&second, &format!("snapshots/{tree}/nested/source.rs"))
                .unwrap(),
            b"fn main() {}\n"
        );
    }

    #[test]
    fn path_and_oid_validation_rejects_unsafe_inputs() {
        for p in [
            "",
            "../x",
            "/x",
            "a/../x",
            "a//b",
            ".git/config",
            "a/.GIT/x",
            "a\\b",
            "x\0y",
        ] {
            assert!(validate_path(p).is_err(), "{p:?}");
        }
        for o in ["HEAD", "--help", "a", "xyz"] {
            assert!(validate_oid(o).is_err());
        }
        assert!(validate_path("nested/space file.rs").is_ok());
        let t = TestRepo::new();
        assert!(t.repo.read_ref("../HEAD").is_err());
        assert!(
            t.repo
                .write_tree(&[TreeEntry {
                    mode: "160000".into(),
                    kind: "commit".into(),
                    oid: "a".repeat(40),
                    name: "submodule".into()
                }])
                .is_err()
        );
    }

    #[test]
    fn bundles_preserve_application_history_objects_and_workspaces_without_user_state() {
        let original = TestRepo::new();
        let restored = TestRepo::new();
        let source = original.tree(&[("code.rs", b"fn original() {}\n")]);
        let log = original.repo.hash_blob(b"validation log").unwrap();
        let head = original
            .repo
            .write_transaction(
                "exported",
                None,
                &vec!["created"],
                &(),
                TransactionObjects {
                    records: &BTreeMap::new(),
                    trees: std::slice::from_ref(&source),
                    blobs: std::slice::from_ref(&log),
                },
            )
            .unwrap();
        let workspace = original
            .repo
            .create_commit(&source, None, "workspace")
            .unwrap();
        let workspace_ref = format!("refs/revqueue/workspaces/{workspace}");
        original
            .repo
            .run(&["update-ref", &workspace_ref, &workspace], &[], None)
            .unwrap();
        let user_tree = restored.tree(&[("user.rs", b"staged\n")]);
        let user_head = restored
            .repo
            .create_commit(&user_tree, None, "user source")
            .unwrap();
        restored
            .repo
            .run(&["update-ref", "HEAD", &user_head], &[], None)
            .unwrap();
        fs::write(restored.root.join("user.rs"), b"dirty working copy\n").unwrap();
        restored.repo.run(&["add", "user.rs"], &[], None).unwrap();
        let index = fs::read(restored.repo.git_dir().join("index")).unwrap();
        let backup = original.root.join("backup.bundle");
        original.repo.export_bundle(&backup).unwrap();
        let bytes = fs::read(&backup).unwrap();
        assert!(original.repo.export_bundle(&backup).is_err());
        assert_eq!(fs::read(&backup).unwrap(), bytes);
        restored.repo.import_bundle(&backup).unwrap();
        restored.repo.import_bundle(&backup).unwrap();
        assert_eq!(
            restored.repo.read_ref("exported").unwrap(),
            Some(head.clone())
        );
        assert_eq!(
            restored.repo.read_file(&source, "code.rs").unwrap(),
            b"fn original() {}\n"
        );
        assert_eq!(restored.repo.read_blob(&log).unwrap(), b"validation log");
        assert_eq!(
            restored
                .repo
                .run(&["rev-parse", &workspace_ref], &[], None)
                .unwrap(),
            format!("{workspace}\n").as_bytes()
        );
        assert_eq!(
            restored
                .repo
                .run(&["rev-parse", "HEAD"], &[], None)
                .unwrap(),
            format!("{user_head}\n").as_bytes()
        );
        assert_eq!(
            fs::read(restored.repo.git_dir().join("index")).unwrap(),
            index
        );
        assert_eq!(
            fs::read(restored.root.join("user.rs")).unwrap(),
            b"dirty working copy\n"
        );
        restored
            .repo
            .run(&["gc", "--prune=now"], &[], None)
            .unwrap();
        assert_eq!(
            restored.repo.read_file(&head, "transaction.json").unwrap(),
            br#"["created"]"#
        );
    }

    #[test]
    fn import_rejects_conflicting_history_atomically_and_foreign_namespaces() {
        let original = TestRepo::new();
        let restored = TestRepo::new();
        original
            .repo
            .write_transaction(
                "a-new",
                None,
                &"new",
                &(),
                TransactionObjects {
                    records: &BTreeMap::new(),
                    trees: &[],
                    blobs: &[],
                },
            )
            .unwrap();
        original
            .repo
            .write_transaction(
                "z-conflict",
                None,
                &"incoming",
                &(),
                TransactionObjects {
                    records: &BTreeMap::new(),
                    trees: &[],
                    blobs: &[],
                },
            )
            .unwrap();
        let existing = restored
            .repo
            .write_transaction(
                "z-conflict",
                None,
                &"existing",
                &(),
                TransactionObjects {
                    records: &BTreeMap::new(),
                    trees: &[],
                    blobs: &[],
                },
            )
            .unwrap();
        let backup = original.root.join("conflict.bundle");
        original.repo.export_bundle(&backup).unwrap();
        assert!(restored.repo.import_bundle(&backup).is_err());
        assert_eq!(restored.repo.read_ref("a-new").unwrap(), None);
        assert_eq!(
            restored.repo.read_ref("z-conflict").unwrap(),
            Some(existing)
        );
        let foreign_head = original
            .repo
            .create_commit(&original.repo.empty_tree().unwrap(), None, "foreign")
            .unwrap();
        original
            .repo
            .run(
                &["update-ref", "refs/heads/foreign", &foreign_head],
                &[],
                None,
            )
            .unwrap();
        let foreign_bundle = original.root.join("foreign.bundle");
        let mut cmd = original.repo.command();
        cmd.args(["bundle", "create"])
            .arg(&foreign_bundle)
            .arg("refs/heads/foreign");
        GitRepository::execute(cmd, &[]).unwrap();
        assert!(restored.repo.import_bundle(&foreign_bundle).is_err());
        assert!(
            restored
                .repo
                .run(&["rev-parse", "--verify", "refs/heads/foreign"], &[], None)
                .is_err()
        );
        assert!(restored.repo.object_kind(&foreign_head).is_err());
    }

    #[test]
    fn linked_worktrees_share_the_canonical_common_directory() {
        let t = TestRepo::new();
        let tree = t.tree(&[("file.rs", b"fn main() {}\n")]);
        let destination = t.root.join("linked");
        crate::source::materialize(&t.repo, &tree, &destination).unwrap();
        let linked = GitRepository::open(&destination).unwrap();
        assert_eq!(t.repo.common_dir(), linked.common_dir());
        assert_eq!(t.repo.common_dir(), t.repo.git_dir());
        assert_ne!(t.repo.git_dir(), linked.git_dir());
        assert_eq!(
            linked.common_dir(),
            fs::canonicalize(t.repo.common_dir()).unwrap()
        );
        assert_eq!(
            crate::source::capture(&linked, &destination, &tree, &["file.rs".into()]).unwrap(),
            tree
        );
    }

    #[test]
    fn import_ref_lock_failure_does_not_partially_install_refs() {
        let original = TestRepo::new();
        let restored = TestRepo::new();
        for goal in ["a-first", "z-last"] {
            original
                .repo
                .write_transaction(
                    goal,
                    None,
                    &goal,
                    &(),
                    TransactionObjects {
                        records: &BTreeMap::new(),
                        trees: &[],
                        blobs: &[],
                    },
                )
                .unwrap();
        }
        let backup = original.root.join("atomic.bundle");
        original.repo.export_bundle(&backup).unwrap();
        let lock = restored
            .repo
            .git_dir()
            .join("refs/revqueue/goals/z-last.lock");
        fs::create_dir_all(lock.parent().unwrap()).unwrap();
        fs::write(&lock, b"held by another writer").unwrap();
        assert!(restored.repo.import_bundle(&backup).is_err());
        assert!(restored.repo.list_goals().unwrap().is_empty());
        assert!(
            !restored
                .repo
                .git_dir()
                .join("refs/revqueue/goals/a-first.lock")
                .exists()
        );
        fs::remove_file(lock).unwrap();
        restored.repo.import_bundle(&backup).unwrap();
        assert_eq!(restored.repo.list_goals().unwrap().len(), 2);
    }

    #[test]
    fn sha256_repositories_retain_full_length_object_ids() {
        let t = TestRepo::new();
        let location = t.root.join("sha256");
        let mut command = t.repo.command();
        command
            .args(["init", "--quiet", "--object-format=sha256"])
            .arg(&location);
        GitRepository::execute(command, &[]).unwrap();
        let repo = GitRepository::open(location).unwrap();
        let source = repo.empty_tree().unwrap();
        assert_eq!(source.len(), 64);
        let head = repo
            .write_transaction(
                "sha256",
                None,
                &(),
                &(),
                TransactionObjects {
                    records: &BTreeMap::new(),
                    trees: &[source],
                    blobs: &[],
                },
            )
            .unwrap();
        assert_eq!(head.len(), 64);
        assert_eq!(repo.read_ref("sha256").unwrap(), Some(head.clone()));
        assert_eq!(repo.history(&head).unwrap(), vec![head]);
    }

    #[test]
    fn competing_ref_writers_have_exactly_one_winner() {
        let t = TestRepo::new();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|value| {
                let repo = t.repo.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    repo.write_transaction(
                        "race",
                        None,
                        &value,
                        &value,
                        TransactionObjects {
                            records: &BTreeMap::new(),
                            trees: &[],
                            blobs: &[],
                        },
                    )
                })
            })
            .collect();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|r| r
                    .as_ref()
                    .err()
                    .is_some_and(|e| e.downcast_ref::<Conflict>().is_some()))
                .count(),
            1
        );
    }
}
