use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, ensure};
use filetime::FileTime;

use super::{Identity, Volume};
use crate::Cancellation;
#[cfg(target_os = "macos")]
use std::{ffi::CStr, os::macos::fs::MetadataExt as _};

pub(crate) fn identity(_path: &Path, metadata: &Metadata) -> Result<Identity> {
    Ok(Identity(metadata.dev(), u128::from(metadata.ino())))
}

pub(crate) fn link_count(_path: &Path, metadata: &Metadata) -> Result<u64> {
    Ok(metadata.nlink())
}

pub(crate) fn validate_type(_path: &Path, _metadata: &Metadata) -> Result<()> {
    Ok(())
}

fn c_path(path: &Path) -> Result<CString> {
    Ok(CString::new(path.as_os_str().as_bytes())?)
}

pub(crate) fn volume(path: &Path) -> Result<Volume> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStringExt;
        let path = fs::canonicalize(path)?;
        let mounts = fs::read("/proc/self/mountinfo")
            .context("cannot inspect mount boundaries through /proc/self/mountinfo")?;
        let mut best = None;
        for line in mounts.split(|byte| *byte == b'\n') {
            let fields: Vec<_> = line.split(|byte| *byte == b' ').take(6).collect();
            if fields.len() < 5 {
                continue;
            }
            let mount = PathBuf::from(std::ffi::OsString::from_vec(unescape_mount(fields[4])));
            if path.starts_with(&mount)
                && best
                    .as_ref()
                    .is_none_or(|(length, _)| mount.as_os_str().len() > *length)
            {
                best = Some((mount.as_os_str().len(), fields[0].to_vec()));
            }
        }
        best.map(|(_, value)| Volume(value))
            .context("cannot identify source mount")
    }
    #[cfg(target_os = "macos")]
    {
        let path = c_path(path)?;
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::statfs(path.as_ptr(), &mut stat) } == 0,
            "cannot inspect volume: {}",
            io::Error::last_os_error()
        );
        Ok(Volume(
            unsafe { CStr::from_ptr(stat.f_mntonname.as_ptr()) }
                .to_bytes()
                .to_vec(),
        ))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    Ok(Volume(fs::metadata(path)?.dev().to_ne_bytes().to_vec()))
}

#[cfg(target_os = "linux")]
fn unescape_mount(value: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'\\'
            && index + 3 < value.len()
            && value[index + 1..index + 4]
                .iter()
                .all(|byte| (b'0'..=b'7').contains(byte))
        {
            result.push(
                (value[index + 1] - b'0') * 64 + (value[index + 2] - b'0') * 8 + value[index + 3]
                    - b'0',
            );
            index += 4;
        } else {
            result.push(value[index]);
            index += 1;
        }
    }
    result
}

pub(crate) fn check_destination(parent: &Path, _directory: bool) -> Result<()> {
    access(parent, libc::W_OK | libc::X_OK)
}

fn access(path: &Path, mode: libc::c_int) -> Result<()> {
    let c = c_path(path)?;
    ensure!(
        unsafe { libc::access(c.as_ptr(), mode) } == 0,
        "insufficient filesystem access to {path:?}: {}",
        io::Error::last_os_error()
    );
    Ok(())
}

pub(crate) type StageDirectory = tempfile::TempDir;

pub(crate) fn stage_directory(parent: &Path) -> Result<StageDirectory> {
    let directory = tempfile::Builder::new()
        .prefix(".unmv-")
        .tempdir_in(parent)?;
    // macOS ACLs can grant access beyond the 0700 mode used by tempfile.
    #[cfg(target_os = "macos")]
    set_acl(directory.path(), &[]).context("cannot make staging directory private")?;
    Ok(directory)
}

pub(crate) fn check_remove(path: &Path, _copy_cleanup: bool) -> Result<()> {
    let parent = path.parent().context("entry has no parent")?;
    access(parent, libc::W_OK | libc::X_OK)?;
    let parent_metadata = fs::metadata(parent)?;
    let metadata = fs::symlink_metadata(path)?;
    let uid = unsafe { libc::geteuid() };
    if parent_metadata.mode() & 0o1000 != 0 {
        ensure!(
            uid == 0 || uid == parent_metadata.uid() || uid == metadata.uid(),
            "sticky directory prevents removing {path:?}"
        );
    }
    #[cfg(target_os = "macos")]
    ensure!(
        metadata.st_flags()
            & (libc::UF_IMMUTABLE | libc::SF_IMMUTABLE | libc::UF_APPEND | libc::SF_APPEND)
            == 0,
        "immutable or append-only entry cannot be removed: {path:?}"
    );
    #[cfg(target_os = "linux")]
    if !metadata.file_type().is_symlink()
        && (metadata.is_file() || metadata.is_dir())
        && let Ok(Some(flags)) = native_flags(path)
    {
        ensure!(
            flags & 0x30 == 0,
            "immutable or append-only entry cannot be removed: {path:?}"
        );
    }
    Ok(())
}

pub(crate) fn open_read(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[derive(Clone, Debug)]
pub(crate) struct Preserved {
    mode: u32,
    uid: u32,
    gid: u32,
    accessed: FileTime,
    modified: FileTime,
    attrs: BTreeMap<Vec<u8>, Vec<u8>>,
    resource_fork: Option<u64>,
    flags: Option<u32>,
    #[cfg(target_os = "macos")]
    acl: Option<Vec<u8>>,
    losses: Vec<String>,
}

impl Preserved {
    pub fn read(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        let mut losses = Vec::new();
        let (attrs, resource_fork) = match attributes(path) {
            Ok(value) => value,
            Err(error) => {
                // A resource fork is data. Inability to enumerate Apple xattrs
                // also prevents proving that this data was copied.
                #[cfg(target_os = "macos")]
                return Err(error).context("cannot inspect extended attributes/resource forks");
                #[cfg(not(target_os = "macos"))]
                {
                    losses.push(format!(
                        "extended attributes could not be inspected: {error:#}"
                    ));
                    (BTreeMap::new(), None)
                }
            }
        };
        ensure!(
            resource_fork.is_none() || metadata.is_file(),
            "resource forks on non-regular entries cannot be copied without following them: {path:?}"
        );
        let flags = if metadata.file_type().is_symlink() {
            None
        } else {
            match native_flags(path) {
                Ok(flags) => flags,
                Err(error) => {
                    losses.push(format!(
                        "native attributes could not be inspected: {error:#}"
                    ));
                    None
                }
            }
        };
        #[cfg(target_os = "macos")]
        let acl = match read_acl(path) {
            Ok(acl) => Some(acl),
            Err(error) => {
                losses.push(format!("ACL could not be inspected: {error:#}"));
                None
            }
        };
        Ok(Self {
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            accessed: FileTime::from_last_access_time(&metadata),
            modified: FileTime::from_last_modification_time(&metadata),
            attrs,
            resource_fork,
            flags,
            #[cfg(target_os = "macos")]
            acl,
            losses,
        })
    }

    pub fn losses(&self) -> &[String] {
        &self.losses
    }

    pub fn extra_bytes(&self) -> u64 {
        self.resource_fork.unwrap_or(0)
    }

    pub fn changed(&self, path: &Path) -> Result<()> {
        let current = Self::read(path)?;
        ensure!(
            self.mode == current.mode
                && self.uid == current.uid
                && self.gid == current.gid
                && self.attrs == current.attrs
                && self.flags == current.flags
                && self.resource_fork == current.resource_fork,
            "source metadata changed since planning: {path:?}"
        );
        #[cfg(target_os = "macos")]
        ensure!(
            self.acl == current.acl,
            "source ACL changed since planning: {path:?}"
        );
        Ok(())
    }

    pub fn apply(&self, path: &Path) -> Vec<String> {
        let mut losses = Vec::new();
        if let Err(error) = self.apply_owner(path) {
            losses.push(format!("ownership: {error:#}"));
        }
        let symlink =
            fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink());
        if !symlink
            && let Err(error) =
                fs::set_permissions(path, fs::Permissions::from_mode(self.mode & 0o7777))
        {
            losses.push(format!("permissions: {error}"));
        }
        #[cfg(target_os = "macos")]
        if symlink {
            match c_path(path) {
                Ok(c)
                    if unsafe {
                        libc::lchmod(c.as_ptr(), (self.mode & 0o7777) as libc::mode_t)
                    } == 0 => {}
                _ => losses.push(format!(
                    "symlink permissions: {}",
                    io::Error::last_os_error()
                )),
            }
        }
        for (name, value) in &self.attrs {
            if let Err(error) = set_attribute(path, name, value) {
                losses.push(format!(
                    "extended attribute {:?}: {error:#}",
                    String::from_utf8_lossy(name)
                ));
            }
        }
        #[cfg(target_os = "macos")]
        if let Some(acl) = &self.acl
            && let Err(error) = set_acl(path, acl)
        {
            losses.push(format!("ACL: {error:#}"));
        }
        if let Err(error) = self.apply_times(path) {
            losses.push(format!("timestamps: {error:#}"));
        }
        if let Some(flags) = self.flags
            && let Err(error) = set_flags(path, flags)
        {
            losses.push(format!("native attributes: {error:#}"));
        }
        match Self::read(path) {
            Ok(actual) => {
                if self.mode != actual.mode {
                    losses.push("permissions differ after copying".into());
                }
                if self.uid != actual.uid || self.gid != actual.gid {
                    losses.push("ownership differs after copying".into());
                }
                if self.accessed != actual.accessed || self.modified != actual.modified {
                    losses.push("timestamps differ after copying".into());
                }
                if self.attrs != actual.attrs {
                    losses.push("extended attributes differ after copying".into());
                }
                if self.flags.is_some() && self.flags != actual.flags {
                    losses.push("native attributes differ after copying".into());
                }
                #[cfg(target_os = "macos")]
                if self.acl != actual.acl {
                    losses.push("ACL differs after copying".into());
                }
                losses.extend(actual.losses);
            }
            Err(error) => losses.push(format!("copied metadata could not be verified: {error:#}")),
        }
        losses.sort();
        losses.dedup();
        losses
    }

    fn apply_owner(&self, path: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.uid() == self.uid && metadata.gid() == self.gid {
            return Ok(());
        }
        let path = c_path(path)?;
        ensure!(
            unsafe { libc::lchown(path.as_ptr(), self.uid, self.gid) } == 0,
            "{}",
            io::Error::last_os_error()
        );
        Ok(())
    }

    pub fn apply_times(&self, path: &Path) -> Result<()> {
        filetime::set_symlink_file_times(path, self.accessed, self.modified)?;
        Ok(())
    }

    pub fn compare_payload(&self, source: &Path, copy: &Path, cancel: &Cancellation) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            ensure!(
                attributes(copy)?.1 == self.resource_fork,
                "resource fork inventory differs: {copy:?}"
            );
            if self.resource_fork.is_some() {
                super::compare_bytes(&fork_path(source), &fork_path(copy), cancel)?;
            }
        }
        let _ = (source, copy, cancel);
        Ok(())
    }

    pub fn copy_payload(&self, source: &Path, copy: &Path, cancel: &Cancellation) -> Result<()> {
        #[cfg(target_os = "macos")]
        if self.resource_fork.is_some() {
            let mut source = open_read(&fork_path(source))?;
            let mut destination = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(fork_path(copy))?;
            transfer(&mut source, &mut destination, cancel)?;
            destination.sync_all()?;
        }
        let _ = (source, copy, cancel);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn fork_path(path: &Path) -> PathBuf {
    path.join("..namedfork/rsrc")
}

type Attributes = BTreeMap<Vec<u8>, Vec<u8>>;

fn attributes(path: &Path) -> Result<(Attributes, Option<u64>)> {
    let path = c_path(path)?;
    let names = xattr_buffer(|buffer, length| unsafe {
        #[cfg(target_os = "linux")]
        {
            libc::llistxattr(path.as_ptr(), buffer.cast(), length)
        }
        #[cfg(target_os = "macos")]
        {
            libc::listxattr(path.as_ptr(), buffer.cast(), length, libc::XATTR_NOFOLLOW)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (buffer, length);
            -1
        }
    })?;
    let mut attrs = BTreeMap::new();
    #[cfg(target_os = "macos")]
    let mut resource_fork = None;
    #[cfg(not(target_os = "macos"))]
    let resource_fork = None;
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let c_name = CString::new(name)?;
        #[cfg(target_os = "macos")]
        if name == b"com.apple.ResourceFork" {
            let size = unsafe {
                libc::getxattr(
                    path.as_ptr(),
                    c_name.as_ptr(),
                    std::ptr::null_mut(),
                    0,
                    0,
                    libc::XATTR_NOFOLLOW,
                )
            };
            ensure!(
                size >= 0,
                "cannot inspect resource fork: {}",
                io::Error::last_os_error()
            );
            resource_fork = Some(size as u64);
            continue;
        }
        let value = xattr_buffer(|buffer, length| unsafe {
            #[cfg(target_os = "linux")]
            {
                libc::lgetxattr(path.as_ptr(), c_name.as_ptr(), buffer.cast(), length)
            }
            #[cfg(target_os = "macos")]
            {
                libc::getxattr(
                    path.as_ptr(),
                    c_name.as_ptr(),
                    buffer.cast(),
                    length,
                    0,
                    libc::XATTR_NOFOLLOW,
                )
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = (buffer, length);
                -1
            }
        })?;
        attrs.insert(name.to_vec(), value);
    }
    Ok((attrs, resource_fork))
}

fn xattr_buffer(mut operation: impl FnMut(*mut u8, usize) -> isize) -> Result<Vec<u8>> {
    for _ in 0..4 {
        let size = operation(std::ptr::null_mut(), 0);
        if size < 0 {
            return Err(io::Error::last_os_error()).context("cannot inspect extended attributes");
        }
        ensure!(
            size <= 64 * 1024 * 1024,
            "extended attribute exceeds the 64 MiB verification limit"
        );
        let mut buffer = vec![0; size as usize];
        let count = operation(buffer.as_mut_ptr(), buffer.len());
        if count >= 0 {
            ensure!(
                count as usize <= buffer.len(),
                "extended attributes changed during inspection"
            );
            buffer.truncate(count as usize);
            return Ok(buffer);
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::ERANGE) {
            return Err(io::Error::last_os_error().into());
        }
    }
    anyhow::bail!("extended attributes keep changing during inspection")
}

fn set_attribute(path: &Path, name: &[u8], value: &[u8]) -> Result<()> {
    let path = c_path(path)?;
    let name = CString::new(name)?;
    let result = unsafe {
        #[cfg(target_os = "linux")]
        {
            libc::lsetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        }
        #[cfg(target_os = "macos")]
        {
            libc::setxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                libc::XATTR_NOFOLLOW,
            )
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            -1
        }
    };
    ensure!(result == 0, "{}", io::Error::last_os_error());
    Ok(())
}

#[cfg(target_os = "macos")]
fn native_flags(path: &Path) -> Result<Option<u32>> {
    Ok(Some(fs::symlink_metadata(path)?.st_flags()))
}

#[cfg(target_os = "linux")]
fn native_flags(path: &Path) -> Result<Option<u32>> {
    let file = open_read(path)?;
    let mut flags: libc::c_long = 0;
    let result = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            linux_raw_sys::ioctl::FS_IOC_GETFLAGS as libc::c_ulong,
            &mut flags,
        )
    };
    if result == 0 {
        return Ok(Some(flags as u32 & !STORAGE_FLAGS));
    }
    let error = io::Error::last_os_error();
    if matches!(error.raw_os_error(), Some(libc::ENOTTY | libc::EOPNOTSUPP)) {
        return Ok(None);
    }
    Err(error.into())
}

#[cfg(target_os = "linux")]
const STORAGE_FLAGS: u32 = 0x0000_1000 | 0x0008_0000 | 0x1000_0000 | 0x4000_0000;

#[cfg(target_os = "macos")]
fn set_flags(path: &Path, flags: u32) -> Result<()> {
    let path = c_path(path)?;
    ensure!(
        unsafe { libc::lchflags(path.as_ptr(), flags) } == 0,
        "{}",
        io::Error::last_os_error()
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_flags(path: &Path, flags: u32) -> Result<()> {
    if native_flags(path)? == Some(flags) {
        return Ok(());
    }
    let file = open_read(path)?;
    let mut raw: libc::c_long = 0;
    ensure!(
        unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                linux_raw_sys::ioctl::FS_IOC_GETFLAGS as libc::c_ulong,
                &mut raw,
            )
        } == 0,
        "{}",
        io::Error::last_os_error()
    );
    raw = ((raw as u32 & STORAGE_FLAGS) | flags) as libc::c_long;
    ensure!(
        unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                linux_raw_sys::ioctl::FS_IOC_SETFLAGS as libc::c_ulong,
                &raw,
            )
        } == 0,
        "{}",
        io::Error::last_os_error()
    );
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn native_flags(_path: &Path) -> Result<Option<u32>> {
    Ok(None)
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn set_flags(_path: &Path, _flags: u32) -> Result<()> {
    anyhow::bail!("native attributes unsupported")
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn acl_get_link_np(path: *const libc::c_char, kind: libc::c_int) -> *mut libc::c_void;
    fn acl_set_link_np(
        path: *const libc::c_char,
        kind: libc::c_int,
        acl: *mut libc::c_void,
    ) -> libc::c_int;
    fn acl_to_text(acl: *mut libc::c_void, length: *mut libc::ssize_t) -> *mut libc::c_char;
    fn acl_from_text(text: *const libc::c_char) -> *mut libc::c_void;
    fn acl_init(count: libc::c_int) -> *mut libc::c_void;
    fn acl_free(object: *mut libc::c_void) -> libc::c_int;
}

#[cfg(target_os = "macos")]
fn read_acl(path: &Path) -> Result<Vec<u8>> {
    let name = c_path(path)?;
    let acl = unsafe { acl_get_link_np(name.as_ptr(), 0x100) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        // Darwin returns ENOENT for the absent FILESEC_ACL property too.
        // Distinguish that from a missing filesystem entry without following links.
        if error.raw_os_error() == Some(libc::ENOENT) {
            fs::symlink_metadata(path)?;
            return Ok(Vec::new());
        }
        return Err(error.into());
    }
    let mut length = 0;
    let text = unsafe { acl_to_text(acl, &mut length) };
    let result: Result<Vec<u8>> = if text.is_null() {
        Err(io::Error::last_os_error().into())
    } else {
        Ok(unsafe { CStr::from_ptr(text) }.to_bytes().to_vec())
    };
    unsafe {
        if !text.is_null() {
            acl_free(text.cast());
        }
        acl_free(acl);
    }
    result
}

#[cfg(target_os = "macos")]
fn set_acl(path: &Path, acl: &[u8]) -> Result<()> {
    if read_acl(path).is_ok_and(|current| current == acl) {
        return Ok(());
    }
    let path = c_path(path)?;
    let text = CString::new(acl)?;
    let acl = unsafe {
        if acl.is_empty() {
            acl_init(0)
        } else {
            acl_from_text(text.as_ptr())
        }
    };
    ensure!(!acl.is_null(), "{}", io::Error::last_os_error());
    let result = unsafe { acl_set_link_np(path.as_ptr(), 0x100, acl) };
    let error = io::Error::last_os_error();
    unsafe {
        acl_free(acl);
    }
    ensure!(result == 0, "{error}");
    Ok(())
}

pub(crate) fn copy_regular(source: &Path, destination: &Path, cancel: &Cancellation) -> Result<()> {
    cancel.check()?;
    let mut source = open_read(source)?;
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)?;
    #[cfg(target_os = "macos")]
    {
        let state = unsafe { libc::copyfile_state_alloc() };
        ensure!(!state.is_null(), "cannot allocate copy state");
        unsafe {
            libc::copyfile_state_set(
                state,
                libc::COPYFILE_STATE_STATUS_CB as u32,
                copy_callback as *const libc::c_void,
            );
            libc::copyfile_state_set(
                state,
                libc::COPYFILE_STATE_STATUS_CTX as u32,
                (cancel as *const Cancellation).cast(),
            );
        }
        let result = unsafe {
            libc::fcopyfile(
                source.as_raw_fd(),
                target.as_raw_fd(),
                state,
                libc::COPYFILE_DATA,
            )
        };
        let error = io::Error::last_os_error();
        unsafe {
            libc::copyfile_state_free(state);
        }
        cancel.check()?;
        ensure!(result == 0, "cannot copy file data: {error}");
        let _ = (&mut source, &mut target);
    }
    #[cfg(not(target_os = "macos"))]
    transfer(&mut source, &mut target, cancel)?;
    target.sync_all()?;
    Ok(())
}

fn transfer(source: &mut File, target: &mut File, cancel: &Cancellation) -> Result<()> {
    use std::io::Read;
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        cancel.check()?;
        let count = source.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        target.write_all(&buffer[..count])?;
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn copy_callback(
    _what: libc::c_int,
    _stage: libc::c_int,
    _state: libc::copyfile_state_t,
    _source: *const libc::c_char,
    _destination: *const libc::c_char,
    context: *mut libc::c_void,
) -> libc::c_int {
    let cancel = unsafe { &*context.cast::<Cancellation>() };
    if cancel.is_cancelled() {
        libc::COPYFILE_QUIT
    } else {
        libc::COPYFILE_CONTINUE
    }
}

pub(crate) fn copy_symlink(source: &Path, destination: &Path) -> Result<()> {
    std::os::unix::fs::symlink(fs::read_link(source)?, destination)?;
    Ok(())
}

pub(crate) fn sync_entry(path: &Path) -> Result<()> {
    if !fs::symlink_metadata(path)?.file_type().is_symlink() {
        open_read(path)?.sync_all()?;
    }
    Ok(())
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}
