use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    ptr,
};

use anyhow::{Context as _, Result, ensure};
use filetime::FileTime;
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, *},
    Storage::FileSystem::*,
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

use super::{Identity, Kind, Volume};
use crate::Cancellation;

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn open_handle(path: &Path, access: u32) -> Result<Handle> {
    let name = wide(path);
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    ensure!(
        handle != INVALID_HANDLE_VALUE,
        "cannot open {path:?}: {}",
        io::Error::last_os_error()
    );
    Ok(Handle(handle))
}

fn information(path: &Path) -> Result<BY_HANDLE_FILE_INFORMATION> {
    let handle = open_handle(path, FILE_READ_ATTRIBUTES)?;
    let mut result = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { GetFileInformationByHandle(handle.0, &mut result) } != 0,
        "{}",
        io::Error::last_os_error()
    );
    Ok(result)
}

pub(crate) fn identity(path: &Path, _metadata: &Metadata) -> Result<Identity> {
    let handle = open_handle(path, FILE_READ_ATTRIBUTES)?;
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe {
            GetFileInformationByHandleEx(
                handle.0,
                FileIdInfo,
                (&mut info as *mut FILE_ID_INFO).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        } != 0,
        "cannot inspect native file identity: {}",
        io::Error::last_os_error()
    );
    Ok(Identity(
        info.VolumeSerialNumber,
        u128::from_le_bytes(info.FileId.Identifier),
    ))
}

pub(crate) fn link_count(path: &Path, _metadata: &Metadata) -> Result<u64> {
    Ok(u64::from(information(path)?.nNumberOfLinks))
}

pub(crate) fn validate_type(path: &Path, metadata: &Metadata) -> Result<()> {
    ensure!(
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
            || metadata.file_type().is_symlink(),
        "unsupported Windows reparse point: {path:?}"
    );
    Ok(())
}

pub(crate) fn volume(path: &Path) -> Result<Volume> {
    let canonical = fs::canonicalize(path)?;
    let name = wide(&canonical);
    let mut mount = vec![0u16; 32768];
    ensure!(
        unsafe { GetVolumePathNameW(name.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) } != 0,
        "cannot identify volume: {}",
        io::Error::last_os_error()
    );
    let mut volume = vec![0u16; 32768];
    if unsafe {
        GetVolumeNameForVolumeMountPointW(mount.as_ptr(), volume.as_mut_ptr(), volume.len() as u32)
    } == 0
    {
        volume = mount;
    }
    volume.truncate(
        volume
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(volume.len()),
    );
    let name = OsString::from_wide(&volume)
        .to_string_lossy()
        .to_lowercase();
    Ok(Volume(name.into_bytes()))
}

pub(crate) fn check_destination(parent: &Path, directory: bool) -> Result<()> {
    open_handle(
        parent,
        FILE_TRAVERSE
            | if directory {
                FILE_ADD_SUBDIRECTORY
            } else {
                FILE_ADD_FILE
            },
    )?;
    Ok(())
}

pub(crate) fn check_remove(path: &Path, copy_cleanup: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        !copy_cleanup
            || metadata.is_dir()
            || metadata.file_attributes() & FILE_ATTRIBUTE_READONLY == 0,
        "read-only entry cannot be deleted: {path:?}"
    );
    if open_handle(path, DELETE).is_ok() {
        return Ok(());
    }
    open_handle(
        path.parent().context("entry has no parent")?,
        FILE_DELETE_CHILD | FILE_TRAVERSE,
    )?;
    Ok(())
}

pub(crate) fn open_read(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AlignedBytes {
    words: Vec<u32>,
    length: usize,
}
impl AlignedBytes {
    unsafe fn read(pointer: *const u8, length: usize) -> Self {
        let mut words = vec![0; length.div_ceil(4)];
        if length != 0 {
            unsafe {
                ptr::copy_nonoverlapping(pointer, words.as_mut_ptr().cast(), length);
            }
        }
        Self { words, length }
    }
    fn pointer(&self) -> *mut std::ffi::c_void {
        if self.length == 0 {
            ptr::null_mut()
        } else {
            self.words.as_ptr().cast_mut().cast()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Security {
    owner: AlignedBytes,
    group: AlignedBytes,
    dacl: AlignedBytes,
    protected: bool,
}

fn read_security(path: &Path) -> Result<Security> {
    let handle = open_handle(path, READ_CONTROL)?;
    let mut owner = ptr::null_mut();
    let mut group = ptr::null_mut();
    let mut dacl = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    let code = unsafe {
        GetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            &mut group,
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    ensure!(code == 0, "{}", io::Error::from_raw_os_error(code as i32));
    let mut control = 0u16;
    let mut revision = 0;
    let valid =
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } != 0;
    let result = unsafe {
        Security {
            owner: AlignedBytes::read(
                owner.cast(),
                if owner.is_null() {
                    0
                } else {
                    GetLengthSid(owner) as usize
                },
            ),
            group: AlignedBytes::read(
                group.cast(),
                if group.is_null() {
                    0
                } else {
                    GetLengthSid(group) as usize
                },
            ),
            dacl: AlignedBytes::read(
                dacl.cast(),
                if dacl.is_null() {
                    0
                } else {
                    (*dacl).AclSize as usize
                },
            ),
            protected: control & SE_DACL_PROTECTED != 0,
        }
    };
    unsafe {
        LocalFree(descriptor);
    }
    ensure!(valid, "cannot inspect security descriptor controls");
    Ok(result)
}

fn apply_security(path: &Path, security: &Security) -> Result<()> {
    let current = read_security(path)?;
    if current == *security {
        return Ok(());
    }
    let mut access = WRITE_DAC;
    let mut flags = DACL_SECURITY_INFORMATION
        | if security.protected {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        };
    if current.owner != security.owner {
        access |= WRITE_OWNER;
        flags |= OWNER_SECURITY_INFORMATION;
    }
    if current.group != security.group {
        access |= WRITE_OWNER;
        flags |= GROUP_SECURITY_INFORMATION;
    }
    let handle = open_handle(path, access)?;
    let code = unsafe {
        SetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            flags,
            security.owner.pointer(),
            security.group.pointer(),
            security.dacl.pointer().cast(),
            ptr::null(),
        )
    };
    ensure!(code == 0, "{}", io::Error::from_raw_os_error(code as i32));
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Audit {
    sacl: AlignedBytes,
    protected: bool,
}

// Do not silently omit privileged security metadata or enable privileges.
// If the caller cannot inspect it, the metadata-loss gate remains closed.
const AUDIT_INFORMATION: u32 = SACL_SECURITY_INFORMATION
    | LABEL_SECURITY_INFORMATION
    | ATTRIBUTE_SECURITY_INFORMATION
    | SCOPE_SECURITY_INFORMATION;
const ACCESS_SYSTEM_SECURITY: u32 = 0x0100_0000;

fn read_audit(path: &Path) -> Result<Audit> {
    let handle = open_handle(path, READ_CONTROL | ACCESS_SYSTEM_SECURITY)?;
    let mut sacl = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    let code = unsafe {
        GetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            AUDIT_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut sacl,
            &mut descriptor,
        )
    };
    ensure!(code == 0, "{}", io::Error::from_raw_os_error(code as i32));
    let mut control = 0;
    let mut revision = 0;
    let valid =
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } != 0;
    let result = Audit {
        sacl: unsafe {
            AlignedBytes::read(
                sacl.cast(),
                if sacl.is_null() {
                    0
                } else {
                    (*sacl).AclSize as usize
                },
            )
        },
        protected: control & SE_SACL_PROTECTED != 0,
    };
    unsafe {
        LocalFree(descriptor);
    }
    ensure!(valid, "cannot inspect SACL controls");
    Ok(result)
}

fn apply_audit(path: &Path, audit: &Audit) -> Result<()> {
    if read_audit(path)? == *audit {
        return Ok(());
    }
    let handle = open_handle(path, ACCESS_SYSTEM_SECURITY)?;
    let flags = AUDIT_INFORMATION
        | if audit.protected {
            PROTECTED_SACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_SACL_SECURITY_INFORMATION
        };
    let code = unsafe {
        SetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            flags,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
            audit.sacl.pointer().cast(),
        )
    };
    ensure!(code == 0, "{}", io::Error::from_raw_os_error(code as i32));
    Ok(())
}

pub(crate) struct StageDirectory(tempfile::NamedTempFile<()>);
impl StageDirectory {
    pub fn path(&self) -> &Path {
        self.0.path()
    }
    pub fn close(self) -> io::Result<()> {
        let path = self.0.into_temp_path().keep()?;
        fs::remove_dir_all(path)
    }
}

pub(crate) fn stage_directory(parent: &Path) -> Result<StageDirectory> {
    let mut token = ptr::null_mut();
    ensure!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } != 0,
        "cannot inspect current token: {}",
        io::Error::last_os_error()
    );
    let token = Handle(token);
    let mut length = 0;
    unsafe {
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut length);
    }
    ensure!(
        length >= std::mem::size_of::<TOKEN_USER>() as u32,
        "cannot inspect current user SID"
    );
    let mut buffer = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
    ensure!(
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        } != 0,
        "cannot inspect current user SID: {}",
        io::Error::last_os_error()
    );
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: user.User.Sid.cast(),
        },
    };
    let mut acl = ptr::null_mut();
    let code = unsafe { SetEntriesInAclW(1, &entry, ptr::null(), &mut acl) };
    ensure!(
        code == 0,
        "cannot construct private DACL: {}",
        io::Error::from_raw_os_error(code as i32)
    );
    let result = (|| -> Result<StageDirectory> {
        let mut descriptor: SECURITY_DESCRIPTOR = unsafe { std::mem::zeroed() };
        let descriptor = (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast();
        ensure!(
            unsafe { InitializeSecurityDescriptor(descriptor, 1) } != 0
                && unsafe { SetSecurityDescriptorDacl(descriptor, 1, acl, 0) } != 0
                && unsafe {
                    SetSecurityDescriptorControl(descriptor, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
                } != 0,
            "cannot construct private security descriptor: {}",
            io::Error::last_os_error()
        );
        let security = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let directory = tempfile::Builder::new()
            .prefix(".unmv-")
            .make_in(parent, |path| {
                let name = wide(path);
                if unsafe { CreateDirectoryW(name.as_ptr(), &security) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            })?;
        Ok(StageDirectory(directory))
    })();
    unsafe {
        LocalFree(acl.cast());
    }
    result
}

type ExtendedAttributes = BTreeMap<Vec<u8>, (u8, Vec<u8>)>;
#[repr(C)]
struct IoStatus {
    status: usize,
    information: usize,
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryEaFile(
        file: HANDLE,
        status: *mut IoStatus,
        buffer: *mut std::ffi::c_void,
        length: u32,
        single: u8,
        list: *const std::ffi::c_void,
        list_length: u32,
        index: *const u32,
        restart: u8,
    ) -> i32;
    fn NtSetEaFile(
        file: HANDLE,
        status: *mut IoStatus,
        buffer: *const std::ffi::c_void,
        length: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}

fn read_ea(path: &Path) -> Result<ExtendedAttributes> {
    let handle = open_handle(path, FILE_READ_EA)?;
    let mut bytes = vec![0u32; 16384];
    let mut result = BTreeMap::new();
    for iteration in 0..1024 {
        let mut status = IoStatus {
            status: 0,
            information: 0,
        };
        let code = unsafe {
            NtQueryEaFile(
                handle.0,
                &mut status,
                bytes.as_mut_ptr().cast(),
                (bytes.len() * 4) as u32,
                0,
                ptr::null(),
                0,
                ptr::null(),
                u8::from(iteration == 0),
            )
        };
        if matches!(code as u32, 0xC000_0052 | 0xC000_004F | 0x8000_0012) {
            return Ok(result);
        }
        ensure!(
            code >= 0 || code as u32 == 0x8000_0005,
            "cannot inspect extended attributes: {}",
            io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(code) } as i32)
        );
        ensure!(
            status.information <= bytes.len() * 4,
            "invalid extended attribute buffer"
        );
        if status.information == 0 {
            return Ok(result);
        }
        let data =
            unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), status.information) };
        let mut offset = 0;
        loop {
            ensure!(
                offset + 8 <= data.len(),
                "invalid extended attribute record"
            );
            let next = u32::from_le_bytes(data[offset..offset + 4].try_into()?) as usize;
            let flags = data[offset + 4];
            let name_length = data[offset + 5] as usize;
            let value_length =
                u16::from_le_bytes(data[offset + 6..offset + 8].try_into()?) as usize;
            let start = offset + 8;
            ensure!(
                start + name_length + 1 + value_length <= data.len(),
                "invalid extended attribute size"
            );
            result.insert(
                data[start..start + name_length].to_vec(),
                (
                    flags,
                    data[start + name_length + 1..start + name_length + 1 + value_length].to_vec(),
                ),
            );
            if next == 0 {
                break;
            }
            ensure!(next >= 8, "invalid extended attribute offset");
            offset += next;
        }
    }
    anyhow::bail!("extended attributes keep changing during inspection")
}

fn apply_ea(path: &Path, attributes: &ExtendedAttributes) -> Result<()> {
    let current = read_ea(path)?;
    if current == *attributes {
        return Ok(());
    }
    let handle = open_handle(path, FILE_WRITE_EA)?;
    let mut combined = attributes.clone();
    for name in current
        .keys()
        .filter(|name| !attributes.contains_key(*name))
    {
        combined.insert(name.clone(), (0, Vec::new()));
    }
    for (name, (flags, value)) in combined {
        ensure!(
            name.len() <= 255 && value.len() <= u16::MAX as usize,
            "extended attribute is too large"
        );
        let length = 8 + name.len() + 1 + value.len();
        let mut words = vec![0u32; length.div_ceil(4)];
        let data = unsafe {
            std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), words.len() * 4)
        };
        data[4] = flags;
        data[5] = name.len() as u8;
        data[6..8].copy_from_slice(&(value.len() as u16).to_le_bytes());
        data[8..8 + name.len()].copy_from_slice(&name);
        data[9 + name.len()..length].copy_from_slice(&value);
        let mut status = IoStatus {
            status: 0,
            information: 0,
        };
        let code =
            unsafe { NtSetEaFile(handle.0, &mut status, words.as_ptr().cast(), length as u32) };
        ensure!(
            code >= 0,
            "cannot retain extended attributes: {}",
            io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(code) } as i32)
        );
    }
    Ok(())
}

fn streams(path: &Path) -> Result<BTreeMap<OsString, u64>> {
    // Query an OPEN_REPARSE_POINT handle so symlink targets are never enumerated.
    let handle = open_handle(path, FILE_READ_ATTRIBUTES)?;
    let mut size = 4096usize;
    loop {
        let mut buffer = vec![0u64; size.div_ceil(8)];
        if unsafe {
            GetFileInformationByHandleEx(
                handle.0,
                FileStreamInfo,
                buffer.as_mut_ptr().cast(),
                size as u32,
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) {
                return Ok(BTreeMap::new());
            }
            if matches!(error.raw_os_error(), Some(234 | 122)) && size < 64 * 1024 * 1024 {
                size *= 2;
                continue;
            }
            return Err(error).context("cannot inspect alternate data streams");
        }
        let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), size) };
        let mut result = BTreeMap::new();
        let mut offset = 0;
        loop {
            const HEADER: usize = std::mem::offset_of!(FILE_STREAM_INFO, StreamName);
            ensure!(
                offset + HEADER <= bytes.len(),
                "invalid stream information buffer"
            );
            let next = u32::from_le_bytes(bytes[offset..offset + 4].try_into()?) as usize;
            let length = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into()?) as usize;
            let stream_size = i64::from_le_bytes(bytes[offset + 8..offset + 16].try_into()?);
            ensure!(
                length.is_multiple_of(2)
                    && stream_size >= 0
                    && offset + HEADER + length <= bytes.len(),
                "invalid stream information record"
            );
            let name: Vec<_> = bytes[offset + HEADER..offset + HEADER + length]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|unit| u16::from_le_bytes(*unit))
                .collect();
            let name = OsString::from_wide(&name);
            if name != "::$DATA" && !name.is_empty() {
                result.insert(name, stream_size as u64);
            }
            if next == 0 {
                return Ok(result);
            }
            ensure!(
                next >= HEADER + length && next.is_multiple_of(8),
                "invalid stream record offset"
            );
            offset += next;
        }
    }
}

fn stream_path(path: &Path, stream: &OsString) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(stream);
    PathBuf::from(name)
}

#[derive(Clone, Debug)]
pub(crate) struct Preserved {
    accessed: FileTime,
    modified: FileTime,
    attributes: u32,
    security: Option<Security>,
    audit: Option<Audit>,
    ea: Option<ExtendedAttributes>,
    streams: BTreeMap<OsString, u64>,
    manual_stream_copy: bool,
    losses: Vec<String>,
}

const LAYOUT_ATTRIBUTES: u32 = FILE_ATTRIBUTE_DIRECTORY
    | FILE_ATTRIBUTE_REPARSE_POINT
    | FILE_ATTRIBUTE_SPARSE_FILE
    | FILE_ATTRIBUTE_COMPRESSED
    | FILE_ATTRIBUTE_NORMAL;

impl Preserved {
    pub fn read(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        let mut losses = Vec::new();
        let security = match read_security(path) {
            Ok(value) => Some(value),
            Err(error) => {
                losses.push(format!("ownership/DACL could not be inspected: {error:#}"));
                None
            }
        };
        let ea = match read_ea(path) {
            Ok(value) => Some(value),
            Err(error) => {
                losses.push(format!(
                    "extended attributes could not be inspected: {error:#}"
                ));
                None
            }
        };
        let audit = match read_audit(path) {
            Ok(value) => Some(value),
            Err(error) => {
                losses.push(format!(
                    "SACL/audit security metadata could not be inspected: {error:#}"
                ));
                None
            }
        };
        let streams = streams(path)?;
        Ok(Self {
            accessed: FileTime::from_last_access_time(&metadata),
            modified: FileTime::from_last_modification_time(&metadata),
            attributes: metadata.file_attributes() & !LAYOUT_ATTRIBUTES,
            security,
            audit,
            ea,
            streams,
            manual_stream_copy: !metadata.is_file() || metadata.file_type().is_symlink(),
            losses,
        })
    }
    pub fn losses(&self) -> &[String] {
        &self.losses
    }
    pub fn extra_bytes(&self) -> u64 {
        self.streams.values().copied().fold(0, u64::saturating_add)
    }
    pub fn changed(&self, path: &Path) -> Result<()> {
        let current = Self::read(path)?;
        ensure!(
            self.attributes == current.attributes
                && self.security == current.security
                && self.audit == current.audit
                && self.ea == current.ea
                && self.streams == current.streams,
            "source metadata or streams changed since planning: {path:?}"
        );
        Ok(())
    }
    pub fn apply(&self, path: &Path) -> Vec<String> {
        let mut losses = Vec::new();
        if let Some(ea) = &self.ea
            && let Err(error) = apply_ea(path, ea)
        {
            losses.push(format!("extended attributes: {error:#}"));
        }
        if let Some(security) = &self.security
            && let Err(error) = apply_security(path, security)
        {
            losses.push(format!("ownership/DACL: {error:#}"));
        }
        if let Some(audit) = &self.audit
            && let Err(error) = apply_audit(path, audit)
        {
            losses.push(format!("SACL/audit security metadata: {error:#}"));
        }
        let name = wide(path);
        if unsafe {
            SetFileAttributesW(
                name.as_ptr(),
                if self.attributes == 0 {
                    FILE_ATTRIBUTE_NORMAL
                } else {
                    self.attributes
                },
            )
        } == 0
        {
            losses.push(format!("native attributes: {}", io::Error::last_os_error()));
        }
        if let Err(error) = self.apply_times(path) {
            losses.push(format!("timestamps: {error:#}"));
        }
        match Self::read(path) {
            Ok(actual) => {
                if self.security.is_some() && self.security != actual.security {
                    losses.push("ownership/DACL differs after copying".into());
                }
                if self.audit.is_some() && self.audit != actual.audit {
                    losses.push("SACL/audit security metadata differs after copying".into());
                }
                if self.ea.is_some() && self.ea != actual.ea {
                    losses.push("extended attributes differ after copying".into());
                }
                if self.attributes != actual.attributes {
                    losses.push("native attributes differ after copying".into());
                }
                if self.accessed != actual.accessed || self.modified != actual.modified {
                    losses.push("timestamps differ after copying".into());
                }
                losses.extend(actual.losses);
            }
            Err(error) => losses.push(format!("copied metadata could not be verified: {error:#}")),
        }
        losses.sort();
        losses.dedup();
        losses
    }
    pub fn apply_times(&self, path: &Path) -> Result<()> {
        filetime::set_symlink_file_times(path, self.accessed, self.modified)?;
        Ok(())
    }
    pub fn compare_payload(
        &self,
        source: &Path,
        copied: &Path,
        cancel: &Cancellation,
    ) -> Result<()> {
        ensure!(
            streams(copied)? == self.streams,
            "alternate data stream inventory differs: {copied:?}"
        );
        for name in self.streams.keys() {
            super::compare_bytes(
                &stream_path(source, name),
                &stream_path(copied, name),
                cancel,
            )?;
        }
        Ok(())
    }
    pub fn copy_payload(&self, source: &Path, copied: &Path, cancel: &Cancellation) -> Result<()> {
        if !self.manual_stream_copy {
            return Ok(());
        }
        for name in self.streams.keys() {
            let mut input = open_read(&stream_path(source, name))?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
                .open(stream_path(copied, name))?;
            let mut buffer = vec![0u8; 1024 * 1024];
            loop {
                cancel.check()?;
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                output.write_all(&buffer[..count])?;
            }
            output.sync_all()?;
        }
        Ok(())
    }
}

pub(crate) fn copy_regular(source: &Path, destination: &Path, cancel: &Cancellation) -> Result<()> {
    let source = wide(source);
    let destination = wide(destination);
    cancel.check()?;
    let result = unsafe {
        CopyFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            Some(copy_progress),
            (cancel as *const Cancellation).cast(),
            ptr::null_mut(),
            COPY_FILE_FAIL_IF_EXISTS | COPY_FILE_COPY_SYMLINK,
        )
    };
    let error = io::Error::last_os_error();
    cancel.check()?;
    ensure!(result != 0, "cannot copy file: {error}");
    Ok(())
}

unsafe extern "system" fn copy_progress(
    _total: i64,
    _copied: i64,
    _stream_size: i64,
    _stream_copied: i64,
    _stream: u32,
    _reason: u32,
    _source: HANDLE,
    _destination: HANDLE,
    context: *const std::ffi::c_void,
) -> u32 {
    if unsafe { &*context.cast::<Cancellation>() }.is_cancelled() {
        PROGRESS_CANCEL
    } else {
        PROGRESS_CONTINUE
    }
}

pub(crate) fn copy_symlink(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    let target = fs::read_link(source)?;
    if metadata.file_type().is_symlink_dir() {
        std::os::windows::fs::symlink_dir(target, destination)?;
    } else {
        std::os::windows::fs::symlink_file(target, destination)?;
    }
    Ok(())
}

pub(crate) fn sync_entry(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if Kind::of(&metadata) == Kind::File {
        OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?
            .sync_all()?;
    }
    Ok(())
}

// Windows has no generally supported directory fsync equivalent. File payloads
// are flushed before cleanup; this is not a power-failure transaction.
pub(crate) fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}
