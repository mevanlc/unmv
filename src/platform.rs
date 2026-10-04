use std::{
    fs::{self, File, Metadata},
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, ensure};

use crate::Cancellation;

#[cfg(unix)]
#[path = "platform/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "platform/windows.rs"]
mod imp;

pub(crate) use imp::*;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Identity(pub u64, pub u128);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Volume(pub Vec<u8>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    File,
    Directory,
    Symlink,
    Other,
}

impl Kind {
    pub fn of(metadata: &Metadata) -> Self {
        if metadata.file_type().is_symlink() {
            Self::Symlink
        } else if metadata.is_dir() {
            Self::Directory
        } else if metadata.is_file() {
            Self::File
        } else {
            Self::Other
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Stamp {
    pub id: Identity,
    pub kind: Kind,
    pub len: u64,
    pub modified: std::time::SystemTime,
    pub link: Option<PathBuf>,
    pub links: u64,
}

impl Stamp {
    pub fn read(path: &Path) -> Result<Self> {
        let metadata =
            fs::symlink_metadata(path).with_context(|| format!("cannot inspect {path:?}"))?;
        validate_type(path, &metadata)?;
        let kind = Kind::of(&metadata);
        Ok(Self {
            id: identity(path, &metadata)?,
            kind,
            len: metadata.len(),
            modified: metadata.modified()?,
            link: if kind == Kind::Symlink {
                Some(fs::read_link(path)?)
            } else {
                None
            },
            links: link_count(path, &metadata)?,
        })
    }

    pub fn check(&self, path: &Path, directory_changed: bool) -> Result<()> {
        let current = Self::read(path)?;
        ensure!(
            self.id == current.id && self.kind == current.kind && self.link == current.link,
            "entry changed since planning: {path:?}"
        );
        if !(directory_changed && self.kind == Kind::Directory) {
            ensure!(
                self.len == current.len && self.modified == current.modified,
                "entry changed since planning: {path:?}"
            );
        }
        Ok(())
    }
}

pub(crate) fn require_missing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => anyhow::bail!("restoration path already exists: {path:?}"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("cannot inspect restoration path {path:?}"))
        }
    }
}

pub(crate) fn children(path: &Path) -> Result<Vec<PathBuf>> {
    let mut result = fs::read_dir(path)
        .with_context(|| format!("cannot read directory {path:?}"))?
        .map(|item| item.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?;
    result.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    Ok(result)
}

pub(crate) fn within_directory(path: &Path, directory: &Path, id: Identity) -> Result<bool> {
    if path.starts_with(directory) {
        return Ok(true);
    }
    // Case aliases and bind-mounted aliases need native identity checks too.
    for ancestor in path.parent().into_iter().flat_map(Path::ancestors) {
        if identity(ancestor, &fs::metadata(ancestor)?)? == id {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn entry_at(root: &Path, relative: &Path) -> PathBuf {
    if relative.as_os_str().is_empty() {
        root.to_owned()
    } else {
        root.join(relative)
    }
}

pub(crate) fn compare_bytes(left: &Path, right: &Path, cancel: &Cancellation) -> Result<()> {
    let mut left_file = open_read(left).with_context(|| format!("cannot read {left:?}"))?;
    let mut right_file = open_read(right).with_context(|| format!("cannot read {right:?}"))?;
    let mut left_buffer = vec![0; 1024 * 1024];
    let mut right_buffer = vec![0; left_buffer.len()];
    loop {
        cancel.check()?;
        let count = read_block(&mut left_file, &mut left_buffer)?;
        let other_count = read_block(&mut right_file, &mut right_buffer)?;
        ensure!(
            count == other_count && left_buffer[..count] == right_buffer[..other_count],
            "copy verification failed: {left:?} differs from {right:?}"
        );
        if count == 0 {
            return Ok(());
        }
    }
}

fn read_block(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut count = 0;
    while count < buffer.len() {
        match file.read(&mut buffer[count..]) {
            Ok(0) => break,
            Ok(bytes) => count += bytes,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(count)
}
