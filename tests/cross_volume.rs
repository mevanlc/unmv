mod common;
use common::*;
use std::{fs, path::Path};
use unmv::{Cancellation, Event, Mode};

#[test]
fn regular_file_restore_preserves_data_and_timestamps() {
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = write(moved.path(), "b", "cross-volume payload");
    let a = root.path().join("a");
    let time = filetime::FileTime::from_unix_time(1234567890, 123000000);
    filetime::set_file_times(&b, time, time).unwrap();
    let plan = plan(&[a.clone(), b.clone()], Mode::Rename);
    unmv::execute(&plan, copy_options(), &Cancellation::default(), |summary| {
        assert!(summary.cross_volume);
        assert_eq!(summary.entries, 1);
        assert!(summary.logical_bytes > 0);
        Ok(true)
    })
    .unwrap();
    let actual = filetime::FileTime::from_last_modification_time(&fs::metadata(&a).unwrap());
    assert_eq!(actual, time);
    assert_eq!(fs::read_to_string(a).unwrap(), "cross-volume payload");
    assert!(!b.exists());
    no_staging(root.path());
}

#[test]
fn directories_symlinks_and_internal_hard_links_are_restored() {
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = moved.path().join("b");
    fs::create_dir(&b).unwrap();
    fs::create_dir(b.join("sub")).unwrap();
    let first = write(&b, "first", "linked payload");
    fs::hard_link(&first, b.join("sub/second")).unwrap();
    symlink(Path::new("missing"), &b.join("dangling"));
    let a = root.path().join("a");
    let plan = plan(&[a.clone(), b.clone()], Mode::Rename);
    #[cfg(unix)]
    assert!(plan.losses.is_empty(), "{:?}", plan.losses);
    unmv::execute(&plan, copy_options(), &Cancellation::default(), |_| {
        Ok(true)
    })
    .unwrap();
    assert_eq!(
        fs::read_link(a.join("dangling")).unwrap(),
        Path::new("missing")
    );
    fs::write(a.join("first"), "changed").unwrap();
    assert_eq!(fs::read_to_string(a.join("sub/second")).unwrap(), "changed");
    assert!(!b.exists());
    no_staging(root.path());
}

#[test]
fn deletion_requires_flag_and_declining_confirmation_cleans_staging() {
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = write(moved.path(), "b", "payload");
    let a = root.path().join("a");
    let plan = plan(&[a.clone(), b.clone()], Mode::Rename);
    assert!(run(&plan).is_err());
    assert!(!a.exists());
    assert!(b.exists());
    no_staging(root.path());
    let error = unmv::execute(&plan, copy_options(), &Cancellation::default(), |_| {
        Ok(false)
    })
    .unwrap_err();
    assert!(format!("{error:#}").contains("cancelled"));
    assert!(!a.exists());
    assert_eq!(fs::read_to_string(b).unwrap(), "payload");
    no_staging(root.path());
}

#[test]
fn copy_corruption_and_source_changes_stop_before_publication() {
    for change_source in [false, true] {
        let Some((root, moved)) = cross_volumes() else {
            return;
        };
        let b = write(moved.path(), "b", "payload");
        let a = root.path().join("a");
        let plan = plan(&[a.clone(), b.clone()], Mode::Rename);
        let result = unmv::execute_with(
            &plan,
            copy_options(),
            &Cancellation::default(),
            |_| Ok(true),
            |event| {
                if let Event::AfterCopy { source, staged } = event {
                    fs::write(if change_source { source } else { staged }, "corrupt")?;
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(!a.exists());
        assert_eq!(
            fs::read_to_string(b).unwrap(),
            if change_source { "corrupt" } else { "payload" }
        );
        no_staging(root.path());
    }
}

#[test]
fn replacing_the_source_after_publication_does_not_delete_the_replacement() {
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = write(moved.path(), "b", "payload");
    let a = root.path().join("a");
    let plan = plan(&[a.clone(), b.clone()], Mode::Rename);
    let result = unmv::execute_with(
        &plan,
        copy_options(),
        &Cancellation::default(),
        |_| Ok(true),
        |event| {
            if let Event::BeforeDelete { source, .. } = event {
                fs::rename(source, moved.path().join("retained"))?;
                fs::write(&b, "replacement")?;
            }
            Ok(())
        },
    );
    assert!(result.is_err());
    assert_eq!(fs::read_to_string(a).unwrap(), "payload");
    assert_eq!(fs::read_to_string(b).unwrap(), "replacement");
    assert_eq!(
        fs::read_to_string(moved.path().join("retained")).unwrap(),
        "payload"
    );
}

#[test]
fn changed_restored_copy_keeps_original_data() {
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = write(moved.path(), "b", "payload");
    let a = root.path().join("a");
    let plan = plan(&[a.clone(), b.clone()], Mode::Rename);
    let result = unmv::execute_with(
        &plan,
        copy_options(),
        &Cancellation::default(),
        |_| Ok(true),
        |event| {
            if let Event::AfterPublish { to } = event {
                fs::write(to, "edited restore")?;
            }
            Ok(())
        },
    );
    assert!(result.is_err());
    assert_eq!(fs::read_to_string(a).unwrap(), "edited restore");
    assert_eq!(fs::read_to_string(b).unwrap(), "payload");
}

#[cfg(unix)]
#[test]
fn restored_metadata_changes_are_not_covered_by_a_loss_waiver() {
    use std::os::unix::fs::PermissionsExt;
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = write(moved.path(), "b", "payload");
    let a = root.path().join("a");
    fs::set_permissions(&b, fs::Permissions::from_mode(0o600)).unwrap();
    let mut options = copy_options();
    options.allow_metadata_loss = true;
    let result = unmv::execute_with(
        &plan(&[a.clone(), b.clone()], Mode::Rename),
        options,
        &Cancellation::default(),
        |_| Ok(true),
        |event| {
            if let Event::AfterPublish { to } = event {
                fs::set_permissions(to, fs::Permissions::from_mode(0o644))?;
            }
            Ok(())
        },
    );
    assert!(result.is_err());
    assert_eq!(fs::read_to_string(a).unwrap(), "payload");
    assert_eq!(
        fs::metadata(&b).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    assert_eq!(fs::read_to_string(b).unwrap(), "payload");
}

#[test]
fn new_directory_entries_are_never_recursively_deleted() {
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = moved.path().join("b");
    fs::create_dir(&b).unwrap();
    write(&b, "child", "payload");
    let a = root.path().join("a");
    let plan = plan(&[a.clone(), b.clone()], Mode::Rename);
    let mut added = false;
    let result = unmv::execute_with(
        &plan,
        copy_options(),
        &Cancellation::default(),
        |_| Ok(true),
        |event| {
            if matches!(event, Event::BeforeDelete { .. }) && !added {
                fs::write(b.join("new"), "new data")?;
                added = true;
            }
            Ok(())
        },
    );
    assert!(result.is_err());
    assert_eq!(fs::read_to_string(a.join("child")).unwrap(), "payload");
    assert_eq!(fs::read_to_string(b.join("new")).unwrap(), "new data");
}

#[test]
fn external_hard_link_loss_requires_separate_opt_in() {
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = write(moved.path(), "b", "payload");
    let sibling = moved.path().join("sibling");
    fs::hard_link(&b, &sibling).unwrap();
    let a = root.path().join("a");
    let plan = plan(&[a.clone(), b.clone()], Mode::Rename);
    assert!(
        plan.losses
            .iter()
            .any(|loss| loss.contains("hard links outside"))
    );
    let mut strict = copy_options();
    strict.allow_metadata_loss = false;
    assert!(unmv::execute(&plan, strict, &Cancellation::default(), |_| Ok(true)).is_err());
    assert!(!a.exists());
    assert!(b.exists());
    let mut options = copy_options();
    options.allow_metadata_loss = true;
    unmv::execute(&plan, options, &Cancellation::default(), |summary| {
        assert!(!summary.losses.is_empty());
        Ok(true)
    })
    .unwrap();
    fs::write(a, "edited").unwrap();
    assert_eq!(fs::read_to_string(sibling).unwrap(), "payload");
    assert!(!b.exists());
}

#[test]
fn staging_failure_leaves_same_volume_members_untouched() {
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = moved.path().join("b");
    fs::create_dir(&b).unwrap();
    write(&b, "native", "native data");
    write(&b, "copied", "copied data");
    let native = moved.path().join("native");
    let copied = root.path().join("copied");
    let plan = plan(&[native.clone(), copied.clone(), b.clone()], Mode::IntoDir);
    let result = unmv::execute_with(
        &plan,
        copy_options(),
        &Cancellation::default(),
        |_| Ok(true),
        |event| {
            if let Event::AfterCopy { staged, .. } = event {
                fs::write(staged, "corrupt")?;
            }
            Ok(())
        },
    );
    assert!(result.is_err());
    assert!(!native.exists());
    assert!(!copied.exists());
    assert_eq!(fs::read_to_string(b.join("native")).unwrap(), "native data");
}

#[cfg(unix)]
#[test]
fn cleaning_unpublished_hard_links_keeps_published_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = moved.path().join("b");
    fs::create_dir(&b).unwrap();
    let first = write(&b, "first", "payload");
    fs::hard_link(&first, b.join("second")).unwrap();
    fs::set_permissions(&first, fs::Permissions::from_mode(0o640)).unwrap();
    let a = root.path().join("first");
    let c = root.path().join("second");
    let result = unmv::execute_with(
        &plan(&[a.clone(), c.clone(), b.clone()], Mode::IntoDir),
        copy_options(),
        &Cancellation::default(),
        |_| Ok(true),
        |event| {
            if matches!(event, Event::AfterPublish { .. }) {
                anyhow::bail!("injected batch interruption");
            }
            Ok(())
        },
    );
    assert!(result.is_err());
    assert!(!c.exists());
    assert_eq!(
        fs::metadata(&a).unwrap().permissions().mode() & 0o7777,
        0o640
    );
    assert_eq!(fs::read_to_string(a).unwrap(), "payload");
    assert_eq!(fs::read_to_string(b.join("first")).unwrap(), "payload");
    assert_eq!(fs::read_to_string(b.join("second")).unwrap(), "payload");
    no_staging(root.path());
}

#[cfg(unix)]
#[test]
fn special_file_in_a_cross_volume_tree_fails_preflight() {
    use std::os::unix::ffi::OsStrExt;
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = moved.path().join("b");
    fs::create_dir(&b).unwrap();
    write(&b, "file", "payload");
    let fifo = std::ffi::CString::new(b.join("fifo").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(
        unmv::infer(
            &[root.path().join("a"), b.clone()],
            Some(Mode::Rename),
            None
        )
        .is_err()
    );
    assert_eq!(fs::read_to_string(b.join("file")).unwrap(), "payload");
    no_staging(root.path());
}

#[test]
fn powershell_cross_volume_directory_move_is_not_inferred() {
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let b = moved.path().join("b");
    fs::create_dir(&b).unwrap();
    assert!(
        unmv::infer(
            &[root.path().join("a"), b],
            Some(Mode::Rename),
            Some(unmv::MvKind::Powershell)
        )
        .is_err()
    );
}

#[cfg(unix)]
#[test]
fn symlink_container_with_blocked_child_does_not_become_a_rename() {
    use std::os::unix::ffi::OsStrExt;
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let fifo = std::ffi::CString::new(moved.path().join("a").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let b = root.path().join("b");
    symlink(moved.path(), &b);
    let result = unmv::infer(&[root.path().join("a"), b.clone()], None, None);
    assert!(result.is_err());
    assert!(format!("{:#}", result.unwrap_err()).contains("choose --mode"));
    assert_eq!(fs::read_link(b).unwrap(), moved.path());
}
