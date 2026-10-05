use std::{
    collections::HashSet,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail, ensure};
use clap::ValueEnum;

use crate::platform::{self, Identity, Kind, Stamp, Volume};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Mode {
    Rename,
    IntoDir,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum MvKind {
    Bsd,
    Gnu,
    Powershell,
}

#[derive(Debug)]
pub enum Inference {
    Certain(Plan),
    Ambiguous(Vec<Plan>),
}

#[derive(Clone, Copy, Debug)]
pub struct Suggestion {
    pub index: usize,
    pub reason: &'static str,
}

impl Inference {
    /// Propose a default for an interactive choice, without resolving ambiguity.
    pub fn suggestion(&self) -> Option<Suggestion> {
        let Self::Ambiguous(plans) = self else {
            return None;
        };
        if plans.len() != 2 {
            return None;
        }
        let rename = plans.iter().find(|plan| plan.mode == Mode::Rename)?;
        let [renamed] = rename.actions.as_slice() else {
            return None;
        };
        if renamed.root.kind != Kind::Directory {
            return None;
        }
        let index = plans.iter().position(|plan| {
            let [moved] = plan.actions.as_slice() else {
                return false;
            };
            plan.mode == Mode::IntoDir && matches!(moved.root.kind, Kind::File | Kind::Symlink)
        })?;
        Some(Suggestion {
            index,
            reason: "a matching file or symlink is inside the destination directory",
        })
    }
}

#[derive(Debug)]
pub struct Plan {
    pub mode: Mode,
    pub actions: Vec<Action>,
    pub losses: Vec<String>,
}

#[derive(Debug)]
pub struct Action {
    pub from: PathBuf,
    pub to: PathBuf,
    pub cross_volume: bool,
    pub(crate) source_parent: Identity,
    pub(crate) destination_parent: Identity,
    pub(crate) destination_volume: Volume,
    pub(crate) root: Stamp,
    pub(crate) nodes: Vec<Node>,
}

#[derive(Debug)]
pub(crate) struct Node {
    pub relative: PathBuf,
    pub stamp: Stamp,
    pub metadata: platform::Preserved,
}

impl Plan {
    pub fn is_cross_volume(&self) -> bool {
        self.actions.iter().any(|action| action.cross_volume)
    }

    pub fn preview(&self) -> String {
        let mut result = String::new();
        for action in &self.actions {
            if action.cross_volume {
                result.push_str(&format!(
                    "copy-and-verify {:?} -> {:?}\n",
                    action.from, action.to
                ));
            } else {
                result.push_str(&format!("rename {:?} -> {:?}\n", action.from, action.to));
            }
        }
        for action in self.actions.iter().filter(|action| action.cross_volume) {
            result.push_str(&format!(
                "delete verified original {:?} (requires --allow-cross-volume-delete)\n",
                action.from
            ));
        }
        for loss in &self.losses {
            result.push_str(&format!(
                "metadata loss: {loss} (requires --allow-metadata-loss)\n"
            ));
        }
        result
    }
}

/// Infer candidate undos, validating every member of each candidate batch.
/// Authorization flags are deliberately excluded: they must not bias inference.
/// With no explicit mode, all plausible interpretations are considered.
pub fn infer(paths: &[PathBuf], mode: Option<Mode>, kind: Option<MvKind>) -> Result<Inference> {
    ensure!(
        paths.len() >= 2,
        "at least one source and one destination are required"
    );
    ensure!(
        mode != Some(Mode::Rename) || paths.len() == 2,
        "--mode rename requires exactly one source"
    );
    if kind == Some(MvKind::Powershell) {
        for path in &paths[..paths.len() - 1] {
            ensure!(
                !path
                    .as_os_str()
                    .to_string_lossy()
                    .contains(['*', '?', '[', ']']),
                "PowerShell wildcard sources are outside v1: {path:?}; supply literal paths"
            );
        }
    }
    let sources = paths[..paths.len() - 1]
        .iter()
        .map(|path| entry_path(path))
        .collect::<Result<Vec<_>>>()?;
    for source in &sources {
        platform::require_missing(source)?;
    }
    let destination_input = &paths[paths.len() - 1];
    let destination = entry_path(destination_input)?;
    let directory_hint =
        trailing_separator(destination_input.as_os_str()) && kind != Some(MvKind::Powershell);
    let modes = match mode {
        Some(mode) => vec![mode],
        None if sources.len() > 1 => vec![Mode::IntoDir],
        None => vec![Mode::IntoDir, Mode::Rename],
    };
    let mut candidates = Vec::new();
    let mut failures = Vec::new();
    let mut blocked = Vec::new();
    for mode in modes {
        if !plausible(mode, &sources, &destination, kind, directory_hint)? {
            continue;
        }
        match candidate(
            mode,
            &sources,
            &destination,
            kind,
            &paths[..paths.len() - 1],
        ) {
            Ok(plan) => candidates.push(plan),
            Err(error) => {
                let message = format!("{mode:?}: {error:#}");
                blocked.push(message.clone());
                failures.push(message);
            }
        }
    }
    if candidates.len() == 1 && !blocked.is_empty() && mode.is_none() {
        bail!(
            "another interpretation is plausible but cannot be safely restored; choose --mode rename or --mode into-dir instead of inferring from a preflight failure:\n{}",
            blocked.join("\n")
        );
    }
    match candidates.len() {
        0 => bail!(
            "cannot infer a safe undo: no matching moved entries or a preflight failed\n{}",
            failures.join("\n")
        ),
        1 => Ok(Inference::Certain(candidates.remove(0))),
        _ => Ok(Inference::Ambiguous(candidates)),
    }
}

fn plausible(
    mode: Mode,
    sources: &[PathBuf],
    destination: &Path,
    kind: Option<MvKind>,
    directory_hint: bool,
) -> Result<bool> {
    let container = if mode == Mode::IntoDir {
        match fs::metadata(destination) {
            Ok(metadata) if metadata.is_dir() => Some(fs::canonicalize(destination)?),
            Ok(_) => return Ok(false),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        }
    } else {
        None
    };
    for source in sources {
        let from = container.as_ref().map_or_else(
            || destination.to_path_buf(),
            |parent| parent.join(source.file_name().expect("entry name")),
        );
        let metadata = match fs::symlink_metadata(&from) {
            Ok(metadata) => metadata,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        };
        // BSD/GNU mv can rename a plain directory to a missing destination
        // ending in '/'. The separator excludes file/symlink renames, but
        // does not prove that a directory container existed before the move.
        if mode == Mode::Rename && directory_hint && Kind::of(&metadata) != Kind::Directory {
            return Ok(false);
        }
        if kind == Some(MvKind::Powershell)
            && Kind::of(&metadata) == Kind::Directory
            && platform::volume(from.parent().expect("entry parent"))?
                != platform::volume(source.parent().expect("entry parent"))?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn candidate(
    mode: Mode,
    sources: &[PathBuf],
    destination: &Path,
    kind: Option<MvKind>,
    original: &[PathBuf],
) -> Result<Plan> {
    let container = if mode == Mode::IntoDir {
        ensure!(
            fs::metadata(destination)?.is_dir(),
            "destination is not a directory container: {destination:?}"
        );
        Some(fs::canonicalize(destination)?)
    } else {
        None
    };
    let mut actions = Vec::new();
    for (source, input) in sources.iter().zip(original) {
        let from = if let Some(container) = &container {
            container.join(source.file_name().expect("entry paths have a final name"))
        } else {
            destination.to_path_buf()
        };
        let root = Stamp::read(&from)?;
        if trailing_separator(input.as_os_str()) {
            ensure!(
                root.kind == Kind::Directory,
                "source with a trailing separator is not a plain directory: {input:?}"
            );
        }
        let from_parent = from.parent().expect("absolute entry parent");
        let to_parent = source.parent().expect("absolute entry parent");
        let source_parent = platform::identity(from_parent, &fs::metadata(from_parent)?)?;
        let destination_parent = platform::identity(to_parent, &fs::metadata(to_parent)?)?;
        let source_volume = platform::volume(from_parent)?;
        let destination_volume = platform::volume(to_parent)?;
        if root.kind == Kind::Directory {
            ensure!(
                !platform::within_directory(source, &from, root.id)?,
                "restoration path is inside the entry to be moved: {source:?}"
            );
            ensure!(
                platform::volume(&from)? == source_volume,
                "cannot move a mounted directory: {from:?}"
            );
        }
        let cross_volume = source_volume != destination_volume;
        ensure!(
            !(cross_volume && root.kind == Kind::Directory && kind == Some(MvKind::Powershell)),
            "PowerShell Move-Item cannot have moved this directory across volumes: {from:?}"
        );
        platform::check_destination(to_parent, root.kind == Kind::Directory)?;
        platform::check_remove(&from, cross_volume)?;
        ensure!(
            renamore::rename_exclusive_is_atomic(to_parent)?,
            "restoration filesystem does not support native no-overwrite renames: {to_parent:?}"
        );
        if !cross_volume {
            ensure!(
                renamore::rename_exclusive_is_atomic(from_parent)?,
                "source filesystem does not support native no-overwrite renames: {from_parent:?}"
            );
        }
        let mut nodes = Vec::new();
        if cross_volume {
            scan(&from, Path::new(""), &source_volume, &mut nodes)?;
        }
        actions.push(Action {
            from,
            to: source.clone(),
            cross_volume,
            source_parent,
            destination_parent,
            destination_volume,
            root,
            nodes,
        });
    }
    validate_overlap(&actions)?;
    let mut losses = Vec::new();
    for action in &actions {
        for node in &action.nodes {
            let path = platform::entry_at(&action.from, &node.relative);
            losses.extend(
                node.metadata
                    .losses()
                    .iter()
                    .map(|loss| format!("{path:?}: {loss}")),
            );
            if node.stamp.kind == Kind::File && node.stamp.links > 1 {
                let matching = actions
                    .iter()
                    .flat_map(|item| &item.nodes)
                    .filter(|other| {
                        other.stamp.kind == Kind::File && other.stamp.id == node.stamp.id
                    })
                    .count() as u64;
                if matching < node.stamp.links {
                    losses.push(format!("{path:?}: hard links outside the restored batch cannot be preserved across volumes"));
                }
                let volumes: HashSet<_> = actions
                    .iter()
                    .filter(|item| {
                        item.nodes
                            .iter()
                            .any(|other| other.stamp.id == node.stamp.id)
                    })
                    .map(|item| &item.destination_volume)
                    .collect();
                if volumes.len() > 1 {
                    losses.push(format!(
                        "{path:?}: hard links spanning restoration volumes cannot be preserved"
                    ));
                }
            }
        }
    }
    losses.sort();
    losses.dedup();
    Ok(Plan {
        mode,
        actions,
        losses,
    })
}

fn scan(root: &Path, relative: &Path, volume: &Volume, nodes: &mut Vec<Node>) -> Result<()> {
    let path = platform::entry_at(root, relative);
    let stamp = Stamp::read(&path)?;
    ensure!(
        stamp.kind != Kind::Other,
        "cross-volume restoration cannot copy this entry type: {path:?}"
    );
    platform::check_remove(&path, true)?;
    if stamp.kind == Kind::Directory {
        ensure!(
            &platform::volume(&path)? == volume,
            "nested mounted directory cannot be copied and deleted: {path:?}"
        );
    }
    let metadata = platform::Preserved::read(&path)?;
    if stamp.kind == Kind::File {
        platform::open_read(&path).with_context(|| format!("cannot read copy source {path:?}"))?;
    }
    nodes.push(Node {
        relative: relative.to_path_buf(),
        stamp: stamp.clone(),
        metadata,
    });
    if stamp.kind == Kind::Directory {
        for child in platform::children(&path)? {
            scan(
                root,
                &relative.join(child.file_name().expect("directory entry name")),
                volume,
                nodes,
            )?;
        }
    }
    Ok(())
}

fn validate_overlap(actions: &[Action]) -> Result<()> {
    for (index, action) in actions.iter().enumerate() {
        for other in &actions[..index] {
            ensure!(
                action.from != other.from && action.to != other.to,
                "duplicate mapping: {:?} or {:?}",
                action.from,
                action.to
            );
            if action.source_parent == other.source_parent && action.root.id == other.root.id {
                let first = action.from.file_name().expect("entry name");
                let second = other.from.file_name().expect("entry name");
                let names: HashSet<_> =
                    platform::children(action.from.parent().expect("entry parent"))?
                        .into_iter()
                        .map(|path| path.file_name().expect("entry name").to_os_string())
                        .collect();
                ensure!(
                    first != second && names.contains(first) && names.contains(second),
                    "duplicate or filesystem-aliased source mapping: {:?} and {:?}",
                    action.from,
                    other.from
                );
            }
            for (path, root, stamp) in [
                (&action.from, &other.from, &other.root),
                (&other.from, &action.from, &action.root),
                (&action.to, &other.from, &other.root),
                (&other.to, &action.from, &action.root),
            ] {
                ensure!(
                    !(stamp.kind == Kind::Directory
                        && platform::within_directory(path, root, stamp.id)?),
                    "overlapping restoration operations: {path:?} is inside {root:?}"
                );
            }
        }
    }
    Ok(())
}

fn entry_path(input: &Path) -> Result<PathBuf> {
    ensure!(!input.as_os_str().is_empty(), "empty paths are not allowed");
    ensure!(
        !terminal_dot(input.as_os_str()),
        "paths ending in . or .. cannot identify a moved entry: {input:?}"
    );
    let absolute = if input.is_absolute() {
        input.to_owned()
    } else {
        std::env::current_dir()?.join(input)
    };
    let name = absolute
        .file_name()
        .context("filesystem roots cannot identify a moved entry")?;
    let parent = absolute.parent().context("entry has no parent")?;
    let parent = fs::canonicalize(parent)
        .with_context(|| format!("cannot resolve existing parent of {input:?}"))?;
    ensure!(
        fs::metadata(&parent)?.is_dir(),
        "entry parent is not a directory: {parent:?}"
    );
    Ok(parent.join(name))
}

fn trailing_separator(value: &OsStr) -> bool {
    value
        .as_encoded_bytes()
        .last()
        .is_some_and(|byte| *byte == b'/' || cfg!(windows) && *byte == b'\\')
}

fn terminal_dot(value: &OsStr) -> bool {
    let mut parts = value
        .as_encoded_bytes()
        .split(|byte| *byte == b'/' || cfg!(windows) && *byte == b'\\')
        .filter(|part| !part.is_empty());
    matches!(parts.next_back(), Some(b"." | b".."))
}
