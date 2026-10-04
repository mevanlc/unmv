mod common;
use common::*;
use std::{fs, path::PathBuf};
use tempfile::TempDir;
use unmv::{Cancellation, Event, ExecutionOptions, Inference, Mode, MvKind};

#[test]
fn renames_files_and_whole_directories_back() {
    for directory in [false, true] {
        let root = TempDir::new().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        if directory {
            fs::create_dir(&a).unwrap();
            write(&a, "child", "payload");
        } else {
            fs::write(&a, "payload").unwrap();
        }
        fs::rename(&a, &b).unwrap();
        let inferred = unmv::infer(&[a.clone(), b.clone()], None, None).unwrap();
        let Inference::Certain(plan) = inferred else {
            panic!("unexpected ambiguity")
        };
        run(&plan).unwrap();
        assert!(!b.exists());
        assert_eq!(
            fs::read_to_string(if directory { a.join("child") } else { a }).unwrap(),
            "payload"
        );
    }
}

#[test]
fn directory_with_matching_child_requires_interpretation() {
    for mode in [Mode::IntoDir, Mode::Rename] {
        let root = TempDir::new().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        fs::create_dir(&b).unwrap();
        write(&b, "a", "matching child");
        write(&b, "other", "other child");
        assert!(matches!(
            unmv::infer(&[a.clone(), b.clone()], None, None).unwrap(),
            Inference::Ambiguous(_)
        ));
        run(&plan(&[a.clone(), b.clone()], mode)).unwrap();
        if mode == Mode::IntoDir {
            assert_eq!(fs::read_to_string(&a).unwrap(), "matching child");
            assert_eq!(fs::read_to_string(b.join("other")).unwrap(), "other child");
        } else {
            assert_eq!(fs::read_to_string(a.join("a")).unwrap(), "matching child");
            assert!(!b.exists());
        }
    }
}

#[test]
fn trailing_destination_separator_does_not_prove_a_container_existed() {
    let root = TempDir::new().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    fs::create_dir(&b).unwrap();
    write(&b, "a", "payload");
    let mut spelled = b.as_os_str().to_os_string();
    spelled.push(std::path::MAIN_SEPARATOR.to_string());
    assert!(matches!(
        unmv::infer(
            &[a.clone(), PathBuf::from(spelled)],
            None,
            Some(MvKind::Gnu),
        )
        .unwrap(),
        Inference::Ambiguous(_)
    ));
    let plan = plan(&[a.clone(), b.clone()], Mode::IntoDir);
    assert_eq!(plan.mode, Mode::IntoDir);
    run(&plan).unwrap();
    assert!(b.is_dir());
    assert!(a.is_file());
}

#[test]
fn trailing_destination_separator_can_undo_a_plain_directory_rename() {
    let root = TempDir::new().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    fs::create_dir(&b).unwrap();
    write(&b, "child", "payload");
    let mut spelled = b.as_os_str().to_os_string();
    spelled.push(std::path::MAIN_SEPARATOR.to_string());
    let Inference::Certain(plan) = unmv::infer(
        &[a.clone(), PathBuf::from(spelled)],
        None,
        Some(MvKind::Gnu),
    )
    .unwrap() else {
        panic!("directory rename is the only matching interpretation");
    };
    assert_eq!(plan.mode, Mode::Rename);
    run(&plan).unwrap();
    assert_eq!(fs::read_to_string(a.join("child")).unwrap(), "payload");
    assert!(!b.exists());
}

#[test]
fn restores_multiple_sources_to_their_original_parents() {
    let root = TempDir::new().unwrap();
    let left = root.path().join("left");
    let right = root.path().join("right");
    let b = root.path().join("b");
    for path in [&left, &right, &b] {
        fs::create_dir(path).unwrap();
    }
    write(&b, "first", "one");
    write(&b, "second", "two");
    let a = left.join("first");
    let c = right.join("second");
    let Inference::Certain(plan) =
        unmv::infer(&[a.clone(), c.clone(), b.clone()], None, None).unwrap()
    else {
        panic!()
    };
    run(&plan).unwrap();
    assert_eq!(fs::read_to_string(a).unwrap(), "one");
    assert_eq!(fs::read_to_string(c).unwrap(), "two");
    assert_eq!(fs::read_dir(b).unwrap().count(), 0);
}

#[test]
fn collision_or_missing_member_refuses_the_entire_batch() {
    for collision in [false, true] {
        let root = TempDir::new().unwrap();
        let b = root.path().join("b");
        fs::create_dir(&b).unwrap();
        write(&b, "a", "one");
        if collision {
            write(&b, "c", "two");
            write(root.path(), "c", "recreated");
        }
        let result = unmv::infer(
            &[root.path().join("a"), root.path().join("c"), b.clone()],
            None,
            None,
        );
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(b.join("a")).unwrap(), "one");
        assert!(!root.path().join("a").exists());
        if collision {
            assert_eq!(
                fs::read_to_string(root.path().join("c")).unwrap(),
                "recreated"
            );
        }
    }
}

#[test]
fn duplicate_basenames_and_missing_parents_are_refused() {
    let root = TempDir::new().unwrap();
    let b = root.path().join("b");
    let p = root.path().join("p");
    let q = root.path().join("q");
    for path in [&b, &p, &q] {
        fs::create_dir(path).unwrap();
    }
    write(&b, "a", "one");
    assert!(unmv::infer(&[p.join("a"), q.join("a"), b.clone()], None, None).is_err());
    assert!(
        unmv::infer(
            &[root.path().join("missing/a"), b.clone()],
            Some(Mode::Rename),
            None
        )
        .is_err()
    );
    assert_eq!(fs::read_to_string(b.join("a")).unwrap(), "one");
}

#[test]
fn filesystem_case_aliases_cannot_duplicate_or_contain_a_restore() {
    let root = TempDir::new().unwrap();
    let b = root.path().join("b");
    fs::create_dir(&b).unwrap();
    write(&b, "a", "payload");
    if fs::symlink_metadata(b.join("A")).is_err() {
        return;
    } // Case-sensitive fixture.
    assert!(
        unmv::infer(
            &[root.path().join("a"), root.path().join("A"), b.clone()],
            None,
            None
        )
        .is_err()
    );
    assert!(
        unmv::infer(
            &[root.path().join("B/new"), b.clone()],
            Some(Mode::Rename),
            None
        )
        .is_err()
    );
    assert!(!root.path().join("a").exists());
    assert_eq!(fs::read_to_string(b.join("a")).unwrap(), "payload");
}

#[test]
fn distinct_hard_link_names_can_be_restored_as_a_batch() {
    let root = TempDir::new().unwrap();
    let b = root.path().join("b");
    fs::create_dir(&b).unwrap();
    let first = write(&b, "first", "payload");
    fs::hard_link(first, b.join("second")).unwrap();
    let a = root.path().join("first");
    let c = root.path().join("second");
    run(&plan(&[a.clone(), c.clone(), b], Mode::IntoDir)).unwrap();
    fs::write(a, "updated").unwrap();
    assert_eq!(fs::read_to_string(c).unwrap(), "updated");
}

#[test]
fn dangling_symlinks_are_entries_and_collisions() {
    let root = TempDir::new().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    symlink(std::path::Path::new("missing-target"), &b);
    run(&plan(&[a.clone(), b.clone()], Mode::Rename)).unwrap();
    assert_eq!(fs::read_link(&a).unwrap(), PathBuf::from("missing-target"));
    write(root.path(), "b", "other");
    assert!(unmv::infer(&[a.clone(), b.clone()], None, None).is_err());
    assert_eq!(fs::read_link(a).unwrap(), PathBuf::from("missing-target"));
    assert_eq!(fs::read_to_string(b).unwrap(), "other");
}

#[cfg(unix)]
#[test]
fn destination_directory_and_original_parent_symlinks_are_preserved() {
    let root = TempDir::new().unwrap();
    let physical = root.path().join("physical");
    let moved = root.path().join("moved");
    fs::create_dir(&physical).unwrap();
    fs::create_dir(&moved).unwrap();
    let original_alias = root.path().join("parent-link");
    let destination_alias = root.path().join("destination-link");
    symlink(&physical, &original_alias);
    symlink(&moved, &destination_alias);
    write(&moved, "a", "payload");
    run(&plan(
        &[original_alias.join("a"), destination_alias.clone()],
        Mode::IntoDir,
    ))
    .unwrap();
    assert_eq!(fs::read_to_string(physical.join("a")).unwrap(), "payload");
    assert_eq!(fs::read_link(original_alias).unwrap(), physical);
    assert_eq!(fs::read_link(destination_alias).unwrap(), moved);
}

#[test]
fn containment_and_terminal_dot_paths_are_refused() {
    let root = TempDir::new().unwrap();
    let b = root.path().join("b");
    fs::create_dir(&b).unwrap();
    write(&b, "child", "payload");
    for source in [
        b.join("new"),
        root.path().join("."),
        root.path().join(".."),
        root.path().join("x/."),
    ] {
        assert!(unmv::infer(&[source, b.clone()], Some(Mode::Rename), None).is_err());
    }
    assert_eq!(fs::read_to_string(b.join("child")).unwrap(), "payload");
}

#[test]
fn literal_brackets_work_but_powershell_wildcards_are_refused() {
    let root = TempDir::new().unwrap();
    let b = write(root.path(), "b", "payload");
    let a = root.path().join("a[1]");
    assert!(unmv::infer(&[a.clone(), b.clone()], None, Some(MvKind::Powershell)).is_err());
    run(&plan(&[a.clone(), b], Mode::Rename)).unwrap();
    assert_eq!(fs::read_to_string(a).unwrap(), "payload");
}

#[test]
fn execution_detects_source_replacement_and_destination_races() {
    for replace_source in [false, true] {
        let root = TempDir::new().unwrap();
        let b = write(root.path(), "b", "payload");
        let a = root.path().join("a");
        let plan = plan(&[a.clone(), b.clone()], Mode::Rename);
        let result = unmv::execute_with(
            &plan,
            ExecutionOptions::default(),
            &Cancellation::default(),
            |_| Ok(true),
            |event| {
                if let Event::BeforePublish { .. } = event {
                    if replace_source {
                        fs::rename(&b, root.path().join("retained"))?;
                        fs::write(&b, "replacement")?;
                    } else {
                        fs::write(&a, "racing destination")?;
                    }
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        if replace_source {
            assert!(!a.exists());
            assert_eq!(fs::read_to_string(&b).unwrap(), "replacement");
        } else {
            assert_eq!(fs::read_to_string(&a).unwrap(), "racing destination");
            assert_eq!(fs::read_to_string(&b).unwrap(), "payload");
        }
    }
}

#[test]
fn runtime_failure_reports_completed_moves_without_deleting_remaining_sources() {
    let root = TempDir::new().unwrap();
    let b = root.path().join("b");
    fs::create_dir(&b).unwrap();
    write(&b, "a", "one");
    write(&b, "c", "two");
    let plan = plan(
        &[root.path().join("a"), root.path().join("c"), b.clone()],
        Mode::IntoDir,
    );
    let result = unmv::execute_with(
        &plan,
        ExecutionOptions::default(),
        &Cancellation::default(),
        |_| Ok(true),
        |event| {
            if matches!(event, Event::AfterPublish { .. }) {
                anyhow::bail!("injected publication failure");
            }
            Ok(())
        },
    );
    let message = format!("{:#}", result.unwrap_err());
    assert!(message.contains("restores published"));
    assert_eq!(fs::read_to_string(root.path().join("a")).unwrap(), "one");
    assert_eq!(fs::read_to_string(b.join("c")).unwrap(), "two");
}

#[test]
fn cancellation_before_execution_keeps_data() {
    let root = TempDir::new().unwrap();
    let b = write(root.path(), "b", "payload");
    let a = root.path().join("a");
    let plan = plan(&[a.clone(), b.clone()], Mode::Rename);
    let cancel = Cancellation::default();
    cancel.cancel();
    assert!(unmv::execute(&plan, ExecutionOptions::default(), &cancel, |_| Ok(true)).is_err());
    assert!(!a.exists());
    assert_eq!(fs::read_to_string(b).unwrap(), "payload");
}

#[test]
fn readonly_files_can_be_renamed_and_hard_links_remain_connected() {
    let root = TempDir::new().unwrap();
    let b = write(root.path(), "b", "payload");
    let other = root.path().join("other");
    let a = root.path().join("a");
    fs::hard_link(&b, &other).unwrap();
    let mut permissions = fs::metadata(&b).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&b, permissions).unwrap();
    run(&plan(&[a.clone(), b], Mode::Rename)).unwrap();
    let mut permissions = fs::metadata(&a).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o600);
    }
    #[cfg(windows)]
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    fs::set_permissions(&a, permissions).unwrap();
    fs::write(&a, "changed").unwrap();
    assert_eq!(fs::read_to_string(other).unwrap(), "changed");
}
