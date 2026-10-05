mod common;
use common::*;
use std::fs;
#[cfg(any(target_os = "macos", windows))]
use unmv::Event;
use unmv::{Cancellation, Mode};

#[cfg(unix)]
fn set_xattr(path: &std::path::Path, name: &str, value: &[u8]) {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let name = CString::new(name).unwrap();
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::lsetxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
}

#[cfg(unix)]
fn get_xattr(path: &std::path::Path, name: &str) -> Vec<u8> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let name = CString::new(name).unwrap();
    let mut value = vec![0; 4096];
    #[cfg(target_os = "macos")]
    let length = unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    #[cfg(target_os = "linux")]
    let length = unsafe {
        libc::lgetxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    assert!(length >= 0, "{}", std::io::Error::last_os_error());
    value.truncate(length as usize);
    value
}

#[cfg(unix)]
#[test]
fn unix_mode_and_extended_attributes_are_preserved() {
    use std::os::unix::fs::PermissionsExt;
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = write(moved.path(), "b", "payload");
    let a = root.path().join("a");
    fs::set_permissions(&b, fs::Permissions::from_mode(0o640)).unwrap();
    let name = if cfg!(target_os = "macos") {
        "com.unmv.test"
    } else {
        "user.unmv.test"
    };
    set_xattr(&b, name, b"binary\0attribute\xff");
    unmv::execute(
        &plan(&[a.clone(), b.clone()], Mode::Rename),
        copy_options(),
        &Cancellation::default(),
        |_| Ok(true),
    )
    .unwrap();
    assert_eq!(
        fs::metadata(&a).unwrap().permissions().mode() & 0o7777,
        0o640
    );
    assert_eq!(get_xattr(&a, name), b"binary\0attribute\xff");
    assert!(!b.exists());
}

#[cfg(target_os = "macos")]
fn mac_acl(path: &std::path::Path) -> Vec<u8> {
    use std::{
        ffi::{CStr, CString},
        os::unix::ffi::OsStrExt,
    };
    unsafe extern "C" {
        fn acl_get_link_np(path: *const libc::c_char, kind: libc::c_int) -> *mut libc::c_void;
        fn acl_to_text(acl: *mut libc::c_void, length: *mut libc::ssize_t) -> *mut libc::c_char;
        fn acl_free(object: *mut libc::c_void) -> libc::c_int;
    }
    let name = CString::new(path.as_os_str().as_bytes()).unwrap();
    let acl = unsafe { acl_get_link_np(name.as_ptr(), 0x100) };
    assert!(!acl.is_null());
    let mut length = 0;
    let text = unsafe { acl_to_text(acl, &mut length) };
    assert!(!text.is_null());
    let value = unsafe { CStr::from_ptr(text) }.to_bytes().to_vec();
    unsafe {
        acl_free(text.cast());
        acl_free(acl);
    }
    value
}

#[cfg(target_os = "macos")]
#[test]
fn macos_acls_native_flags_and_resource_forks_are_preserved() {
    use std::{os::unix::ffi::OsStrExt, process::Command};
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = write(moved.path(), "b", "data fork");
    let a = root.path().join("a");
    set_xattr(&b, "com.apple.ResourceFork", b"resource fork\0data");
    assert!(
        Command::new("chmod")
            .args(["+a", "everyone allow read"])
            .arg(&b)
            .status()
            .unwrap()
            .success()
    );
    let acl = mac_acl(&b);
    let name = std::ffi::CString::new(b.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::lchflags(name.as_ptr(), libc::UF_HIDDEN) }, 0);
    unmv::execute(
        &plan(&[a.clone(), b.clone()], Mode::Rename),
        copy_options(),
        &Cancellation::default(),
        |_| Ok(true),
    )
    .unwrap();
    assert_eq!(
        get_xattr(&a, "com.apple.ResourceFork"),
        b"resource fork\0data"
    );
    assert_eq!(mac_acl(&a), acl);
    use std::os::macos::fs::MetadataExt;
    assert_ne!(
        fs::symlink_metadata(&a).unwrap().st_flags() & libc::UF_HIDDEN,
        0
    );
    assert_eq!(fs::read_to_string(a).unwrap(), "data fork");
    assert!(!b.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn corrupted_resource_forks_cannot_be_waived_as_metadata_loss() {
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = write(moved.path(), "b", "data fork");
    set_xattr(&b, "com.apple.ResourceFork", b"original resource fork");
    let a = root.path().join("a");
    let mut options = copy_options();
    options.allow_metadata_loss = true;
    let result = unmv::execute_with(
        &plan(&[a.clone(), b.clone()], Mode::Rename),
        options,
        &Cancellation::default(),
        |_| Ok(true),
        |event| {
            if let Event::AfterCopy { staged, .. } = event {
                set_xattr(&staged, "com.apple.ResourceFork", b"corrupt resource fork");
            }
            Ok(())
        },
    );
    assert!(result.is_err());
    assert!(!a.exists());
    assert_eq!(
        get_xattr(&b, "com.apple.ResourceFork"),
        b"original resource fork"
    );
    no_staging(root.path());
}

#[cfg(target_os = "macos")]
#[test]
fn inherited_destination_acls_do_not_silently_change_the_restored_file() {
    use std::process::Command;
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    assert!(
        Command::new("chmod")
            .args(["+a", "everyone allow read,file_inherit,directory_inherit"])
            .arg(root.path())
            .status()
            .unwrap()
            .success()
    );
    let b = write(moved.path(), "b", "payload");
    let a = root.path().join("a");
    unmv::execute(
        &plan(&[a.clone(), b], Mode::Rename),
        copy_options(),
        &Cancellation::default(),
        |_| Ok(true),
    )
    .unwrap();
    assert_eq!(fs::read_to_string(a).unwrap(), "payload");
    no_staging(root.path());
}

#[cfg(target_os = "linux")]
#[test]
fn linux_posix_acl_xattrs_are_preserved() {
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = write(moved.path(), "b", "payload");
    let a = root.path().join("a");
    // Linux ACL xattr ABI: version 2, owner, named user, group, mask, other.
    let mut acl = 2u32.to_le_bytes().to_vec();
    for (tag, permissions, id) in [
        (1u16, 6u16, u32::MAX),
        (2, 4, 42424),
        (4, 4, u32::MAX),
        (16, 4, u32::MAX),
        (32, 0, u32::MAX),
    ] {
        acl.extend(tag.to_le_bytes());
        acl.extend(permissions.to_le_bytes());
        acl.extend(id.to_le_bytes());
    }
    set_xattr(&b, "system.posix_acl_access", &acl);
    let acl = get_xattr(&b, "system.posix_acl_access");
    unmv::execute(
        &plan(&[a.clone(), b], Mode::Rename),
        copy_options(),
        &Cancellation::default(),
        |_| Ok(true),
    )
    .unwrap();
    assert_eq!(get_xattr(&a, "system.posix_acl_access"), acl);
}

#[cfg(windows)]
#[test]
fn windows_file_and_directory_data_streams_are_verified() {
    for directory in [false, true] {
        let Some((root, moved)) = cross_volumes() else {
            return;
        };
        let b = moved.path().join("b");
        let a = root.path().join("a");
        if directory {
            fs::create_dir(&b).unwrap();
            write(&b, "child", "payload");
        } else {
            fs::write(&b, "payload").unwrap();
        }
        fs::write(stream_path(&b, "unmv-test"), b"alternate\0data").unwrap();
        unmv::execute(
            &plan(&[a.clone(), b.clone()], Mode::Rename),
            copy_options(),
            &Cancellation::default(),
            |_| Ok(true),
        )
        .unwrap();
        assert_eq!(
            fs::read(stream_path(&a, "unmv-test")).unwrap(),
            b"alternate\0data"
        );
        assert!(!b.exists());
        no_staging(root.path());
    }
}

#[cfg(windows)]
#[test]
fn windows_missing_stream_payload_is_never_metadata_loss() {
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = write(moved.path(), "b", "payload");
    let a = root.path().join("a");
    fs::write(stream_path(&b, "unmv-test"), b"alternate data").unwrap();
    let result = unmv::execute_with(
        &plan(&[a.clone(), b.clone()], Mode::Rename),
        copy_options(),
        &Cancellation::default(),
        |_| Ok(true),
        |event| {
            if let Event::AfterCopy { staged, .. } = event {
                fs::remove_file(stream_path(&staged, "unmv-test"))?;
            }
            Ok(())
        },
    );
    assert!(result.is_err());
    assert!(!a.exists());
    assert_eq!(
        fs::read(stream_path(&b, "unmv-test")).unwrap(),
        b"alternate data"
    );
    no_staging(root.path());
}

#[cfg(windows)]
#[test]
fn windows_uninspected_security_requires_metadata_authorization() {
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = write(moved.path(), "b", "payload");
    let a = root.path().join("a");
    let plan = plan(&[a.clone(), b.clone()], Mode::Rename);
    if plan.losses.is_empty() {
        return;
    } // An audit-privileged test runner can inspect the SACL.
    let mut options = copy_options();
    options.allow_metadata_loss = false;
    let error = unmv::execute(&plan, options, &Cancellation::default(), |_| Ok(true)).unwrap_err();
    assert!(format!("{error:#}").contains("--allow-metadata-loss"));
    assert!(!a.exists());
    assert!(b.exists());
    no_staging(root.path());
}
