use crate::{
    Cancellation, Plan,
    plan::{Action, Node},
    platform::{self, Identity, Kind, Volume},
};
use anyhow::{Context as _, Result, ensure};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default)]
pub struct ExecutionOptions {
    pub allow_cross_volume_delete: bool,
    pub allow_metadata_loss: bool,
}

#[derive(Debug)]
pub struct Summary {
    pub preview: String,
    pub entries: usize,
    pub logical_bytes: u64,
    pub losses: Vec<String>,
    pub cross_volume: bool,
}

/// Observation points for cancellation and exercising filesystem changes in tests.
#[derive(Clone, Debug)]
pub enum Event {
    BeforeCopy { source: PathBuf, staged: PathBuf },
    AfterCopy { source: PathBuf, staged: PathBuf },
    BeforePublish { from: PathBuf, to: PathBuf },
    AfterPublish { to: PathBuf },
    BeforeDelete { source: PathBuf, restored: PathBuf },
}

pub fn execute(
    plan: &Plan,
    options: ExecutionOptions,
    cancel: &Cancellation,
    confirm: impl FnMut(&Summary) -> Result<bool>,
) -> Result<()> {
    execute_with(plan, options, cancel, confirm, |_| Ok(()))
}

pub fn execute_with(
    plan: &Plan,
    options: ExecutionOptions,
    cancel: &Cancellation,
    mut confirm: impl FnMut(&Summary) -> Result<bool>,
    mut observe: impl FnMut(Event) -> Result<()>,
) -> Result<()> {
    ensure!(
        !plan.is_cross_volume() || options.allow_cross_volume_delete,
        "cross-volume restoration requires --allow-cross-volume-delete; preview with --dry-run"
    );
    ensure!(
        plan.losses.is_empty() || options.allow_metadata_loss,
        "metadata preservation would be incomplete; requires --allow-metadata-loss:\n{}",
        plan.losses.join("\n")
    );
    let mut stages: Vec<Option<Stage>> = (0..plan.actions.len()).map(|_| None).collect();
    let mut published = Vec::new();
    let mut deleted = HashSet::new();
    let result = (|| -> Result<()> {
        cancel.check()?;
        for action in &plan.actions {
            revalidate(action)?;
        }
        let mut links: HashMap<(Identity, Volume), PathBuf> = HashMap::new();
        let mut losses = plan.losses.clone();
        for (index, action) in plan.actions.iter().enumerate() {
            if !action.cross_volume {
                continue;
            }
            cancel.check()?;
            let mut stage = Stage::new(action.to.parent().expect("restoration parent"))?;
            observe(Event::BeforeCopy {
                source: action.from.clone(),
                staged: stage.entry.clone(),
            })?;
            copy_tree(action, &stage.entry, &mut links, cancel)?;
            observe(Event::AfterCopy {
                source: action.from.clone(),
                staged: stage.entry.clone(),
            })?;
            verify_manifest(action)?;
            verify_data(action, &stage.entry, cancel)?;
            for node in action.nodes.iter().rev() {
                let copied = platform::entry_at(&stage.entry, &node.relative);
                losses.extend(node.metadata.apply(&copied).into_iter().map(|loss| {
                    format!(
                        "{:?}: {loss}",
                        platform::entry_at(&action.from, &node.relative)
                    )
                }));
                platform::sync_entry(&copied)?;
            }
            // Remember the actual copies, including any explicitly authorized
            // metadata differences. Later changes are not covered by that waiver.
            for node in &action.nodes {
                let copied = platform::entry_at(&stage.entry, &node.relative);
                stage.nodes.push(Node {
                    relative: node.relative.clone(),
                    stamp: platform::Stamp::read(&copied)?,
                    metadata: platform::Preserved::read(&copied)?,
                });
            }
            stages[index] = Some(stage);
        }
        losses.sort();
        losses.dedup();
        ensure!(
            losses.is_empty() || options.allow_metadata_loss,
            "staged copies could not preserve metadata; originals retained; requires --allow-metadata-loss:\n{}",
            losses.join("\n")
        );
        let summary = Summary {
            preview: plan.preview(),
            entries: plan.actions.iter().map(|action| action.nodes.len()).sum(),
            logical_bytes: plan
                .actions
                .iter()
                .flat_map(|action| &action.nodes)
                .map(|node| {
                    (if node.stamp.kind == Kind::File {
                        node.stamp.len
                    } else {
                        0
                    })
                    .saturating_add(node.metadata.extra_bytes())
                })
                .fold(0, u64::saturating_add),
            losses,
            cross_volume: plan.is_cross_volume(),
        };
        if summary.cross_volume {
            ensure!(
                confirm(&summary)?,
                "operation cancelled before publication; originals retained"
            );
        }
        for action in &plan.actions {
            revalidate(action)?;
        }
        cancel.check()?;
        // Publish all restores before deleting any copied original.
        for (index, action) in plan.actions.iter().enumerate() {
            let from = stages[index]
                .as_ref()
                .map_or(&action.from, |stage| &stage.entry);
            observe(Event::BeforePublish {
                from: from.clone(),
                to: action.to.clone(),
            })?;
            cancel.check()?;
            revalidate(action)?;
            if let Some(stage) = &stages[index] {
                stage.check(from)?;
            }
            renamore::rename_exclusive(from, &action.to)
                .with_context(|| format!("cannot publish restore {from:?} -> {:?}", action.to))?;
            published.push(action.to.clone());
            platform::sync_directory(action.to.parent().expect("restoration parent"))?;
            observe(Event::AfterPublish {
                to: action.to.clone(),
            })?;
        }
        for (action, stage) in plan.actions.iter().zip(&stages) {
            if let Some(stage) = stage {
                stage.check(&action.to)?;
                verify_manifest(action)?;
                verify_data(action, &action.to, cancel)?;
            }
        }
        for (action, stage) in plan.actions.iter().zip(&stages) {
            let Some(stage) = stage else {
                continue;
            };
            for (node, copied_node) in action.nodes.iter().zip(&stage.nodes).rev() {
                cancel.check()?;
                let source = platform::entry_at(&action.from, &node.relative);
                let restored = platform::entry_at(&action.to, &node.relative);
                observe(Event::BeforeDelete {
                    source: source.clone(),
                    restored: restored.clone(),
                })?;
                cancel.check()?;
                copied_node.stamp.check(&restored, false)?;
                copied_node.metadata.changed(&restored)?;
                node.stamp.check(&source, true)?;
                if node.stamp.kind != Kind::Directory {
                    node.metadata.changed(&source)?;
                    verify_node(node, &source, &restored, cancel)?;
                }
                node.metadata.apply_times(&restored).with_context(|| {
                    format!("cannot retain copied timestamps before deleting {source:?}")
                })?;
                if node.stamp.kind == Kind::Directory {
                    fs::remove_dir(&source).with_context(|| {
                        format!("cannot remove verified empty directory {source:?}")
                    })?;
                } else {
                    fs::remove_file(&source)
                        .with_context(|| format!("cannot delete verified original {source:?}"))?;
                }
            }
            deleted.insert(action.from.clone());
            platform::sync_directory(action.from.parent().expect("source parent"))?;
        }
        Ok(())
    })();
    result.with_context(|| {
        let remaining: Vec<_> = plan
            .actions
            .iter()
            .filter(|action| {
                !deleted.contains(&action.from) && fs::symlink_metadata(&action.from).is_ok()
            })
            .map(|action| &action.from)
            .collect();
        format!("restores published: {published:?}; source entries retained: {remaining:?}")
    })
}

struct Stage {
    directory: Option<platform::StageDirectory>,
    entry: PathBuf,
    nodes: Vec<Node>,
}

impl Stage {
    fn new(parent: &Path) -> Result<Self> {
        let directory = platform::stage_directory(parent)?;
        let entry = directory.path().join("entry");
        Ok(Self {
            directory: Some(directory),
            entry,
            nodes: Vec::new(),
        })
    }

    fn check(&self, root: &Path) -> Result<()> {
        for node in &self.nodes {
            let copied = platform::entry_at(root, &node.relative);
            node.stamp.check(&copied, false)?;
            node.metadata.changed(&copied)?;
        }
        Ok(())
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if let Some(directory) = self.directory.take() {
            let path = directory.path().to_path_buf();
            make_stage_removable(&self.entry);
            if let Err(error) = directory.close() {
                eprintln!("unmv: staging directory retained at {path:?}: {error}");
            }
        }
    }
}

fn make_stage_removable(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_symlink() {
        return;
    }
    let mut permissions = metadata.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // A staged file can share its inode with an already published restore.
        // Unlinking it needs write access to the parent, not to the file.
        if metadata.is_dir() {
            permissions.set_mode(permissions.mode() | 0o700);
            let _ = fs::set_permissions(path, permissions);
        }
    }
    #[cfg(windows)]
    if permissions.readonly() && platform::link_count(path, &metadata).is_ok_and(|count| count <= 1)
    {
        #[allow(clippy::permissions_set_readonly_false)] // Windows attribute only.
        permissions.set_readonly(false);
        let _ = fs::set_permissions(path, permissions);
    }
    if metadata.is_dir()
        && let Ok(children) = platform::children(path)
    {
        for child in children {
            make_stage_removable(&child);
        }
    }
}

fn revalidate(action: &Action) -> Result<()> {
    action.root.check(&action.from, false)?;
    let source_parent = action.from.parent().expect("source parent");
    let destination_parent = action.to.parent().expect("restoration parent");
    ensure!(
        platform::identity(source_parent, &fs::metadata(source_parent)?)? == action.source_parent,
        "source parent changed since planning: {source_parent:?}"
    );
    ensure!(
        platform::identity(destination_parent, &fs::metadata(destination_parent)?)?
            == action.destination_parent,
        "restoration parent changed since planning: {destination_parent:?}"
    );
    ensure!(
        platform::volume(destination_parent)? == action.destination_volume,
        "restoration volume changed since planning"
    );
    platform::require_missing(&action.to)?;
    if action.cross_volume {
        verify_manifest(action)?;
    }
    Ok(())
}

fn copy_tree(
    action: &Action,
    root: &Path,
    links: &mut HashMap<(Identity, Volume), PathBuf>,
    cancel: &Cancellation,
) -> Result<()> {
    for node in &action.nodes {
        cancel.check()?;
        let source = platform::entry_at(&action.from, &node.relative);
        let copied = platform::entry_at(root, &node.relative);
        node.stamp.check(&source, false)?;
        match node.stamp.kind {
            Kind::Directory => fs::create_dir(&copied)?,
            Kind::Symlink => platform::copy_symlink(&source, &copied)?,
            Kind::File => {
                let key = (node.stamp.id, action.destination_volume.clone());
                if let Some(first) = links.get(&key) {
                    fs::hard_link(first, &copied)
                        .context("cannot preserve an internal hard-link relationship")?;
                } else {
                    platform::copy_regular(&source, &copied, cancel)?;
                    links.insert(key, copied.clone());
                }
            }
            Kind::Other => unreachable!("preflight excludes unsupported copy types"),
        }
        node.metadata.copy_payload(&source, &copied, cancel)?;
    }
    Ok(())
}

fn verify_manifest(action: &Action) -> Result<()> {
    for node in &action.nodes {
        let source = platform::entry_at(&action.from, &node.relative);
        node.stamp.check(&source, false)?;
        node.metadata.changed(&source)?;
        if node.stamp.kind == Kind::Directory {
            let current: HashSet<_> = platform::children(&source)?
                .into_iter()
                .map(|path| path.file_name().expect("entry name").to_os_string())
                .collect();
            let expected: HashSet<_> = action
                .nodes
                .iter()
                .filter(|child| child.relative.parent() == Some(node.relative.as_path()))
                .map(|child| {
                    child
                        .relative
                        .file_name()
                        .expect("child name")
                        .to_os_string()
                })
                .collect();
            ensure!(
                current == expected,
                "source directory inventory changed since planning: {source:?}"
            );
        }
    }
    Ok(())
}

fn verify_data(action: &Action, root: &Path, cancel: &Cancellation) -> Result<()> {
    for node in &action.nodes {
        cancel.check()?;
        let source = platform::entry_at(&action.from, &node.relative);
        let copied = platform::entry_at(root, &node.relative);
        verify_node(node, &source, &copied, cancel)?;
        node.stamp.check(&source, false)?;
    }
    Ok(())
}

fn verify_node(
    node: &crate::plan::Node,
    source: &Path,
    copied: &Path,
    cancel: &Cancellation,
) -> Result<()> {
    let metadata = fs::symlink_metadata(copied)
        .with_context(|| format!("restored copy is missing: {copied:?}"))?;
    ensure!(
        Kind::of(&metadata) == node.stamp.kind,
        "copy entry type differs: {copied:?}"
    );
    match node.stamp.kind {
        Kind::File => {
            platform::compare_bytes(source, copied, cancel)?;
        }
        Kind::Symlink => ensure!(
            fs::read_link(copied)? == node.stamp.link.as_ref().expect("symlink target").as_path(),
            "copied symlink target differs: {copied:?}"
        ),
        Kind::Directory => {}
        Kind::Other => unreachable!(),
    }
    node.metadata.compare_payload(source, copied, cancel)?;
    Ok(())
}
