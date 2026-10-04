mod common;
use common::*;
use std::fs;
use tempfile::TempDir;

#[test]
fn help_version_and_argument_errors_are_consistent() {
    let root = TempDir::new().unwrap();
    let help = ok(command(root.path()).arg("--help").output().unwrap());
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(text.contains("SOURCE"));
    assert!(text.contains("--allow-cross-volume-delete"));
    ok(command(root.path()).arg("--version").output().unwrap());
    for args in [vec![], vec!["a"], vec!["--mode", "rename", "a", "c", "b"]] {
        assert_eq!(
            command(root.path())
                .args(args)
                .output()
                .unwrap()
                .status
                .code(),
            Some(2)
        );
    }
}

#[test]
fn dry_run_and_execution_use_the_same_plan() {
    let root = TempDir::new().unwrap();
    write(root.path(), "b", "payload");
    let preview = ok(command(root.path())
        .args(["--dry-run", "a", "b"])
        .output()
        .unwrap());
    assert!(
        String::from_utf8(preview.stdout)
            .unwrap()
            .contains("rename")
    );
    assert!(!root.path().join("a").exists());
    assert!(root.path().join("b").exists());
    let output = ok(command(root.path()).args(["a", "b"]).output().unwrap());
    assert!(output.stdout.is_empty());
    assert_eq!(
        fs::read_to_string(root.path().join("a")).unwrap(),
        "payload"
    );
}

#[test]
fn ambiguity_requires_a_choice_even_with_yes_or_piped_input() {
    let root = TempDir::new().unwrap();
    let b = root.path().join("b");
    fs::create_dir(&b).unwrap();
    write(&b, "a", "payload");
    for args in [
        vec!["a", "b"],
        vec!["--yes", "a", "b"],
        vec!["--dry-run", "a", "b"],
    ] {
        let output = command(root.path()).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("more than one undo")
        );
        assert!(!root.path().join("a").exists());
    }
    ok(command(root.path())
        .args(["--mode", "into-dir", "a", "b"])
        .output()
        .unwrap());
    assert!(b.is_dir());
    assert!(root.path().join("a").is_file());
}

#[test]
fn unusual_literal_names_and_double_dash_round_trip() {
    let root = TempDir::new().unwrap();
    write(root.path(), "b space ' name", "payload");
    ok(command(root.path())
        .args(["--", "-a space ' name", "b space ' name"])
        .output()
        .unwrap());
    assert_eq!(
        fs::read_to_string(root.path().join("-a space ' name")).unwrap(),
        "payload"
    );
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStringExt;
        let name = std::ffi::OsString::from_vec(vec![b'a', 0xff]);
        let b = write(root.path(), "b", "binary filename");
        ok(command(root.path()).arg(&name).arg(b).output().unwrap());
        assert_eq!(
            fs::read(root.path().join(name)).unwrap(),
            b"binary filename"
        );
    }
}

#[test]
fn unknown_shell_context_requires_override_for_execution() {
    let root = TempDir::new().unwrap();
    write(root.path(), "b", "payload");
    let output = command(root.path())
        .env("UNMV_CONTEXT_TYPE", "unknown")
        .env(
            "UNMV_CONTEXT_DESCRIPTION",
            "function mv { arbitrary behavior }",
        )
        .args(["a", "b"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("--mv-kind")
    );
    ok(command(root.path())
        .env("UNMV_CONTEXT_TYPE", "unknown")
        .args(["--mv-kind", "gnu", "a", "b"])
        .output()
        .unwrap());
    assert_eq!(
        fs::read_to_string(root.path().join("a")).unwrap(),
        "payload"
    );
}

#[test]
fn cross_volume_preview_needs_no_authorization_and_yes_cannot_grant_it() {
    let Some((root, moved)) = cross_volumes() else {
        return;
    };
    let a = root.path().join("a");
    let b = write(moved.path(), "b", "payload");
    let output = ok(command(root.path())
        .arg("--dry-run")
        .arg(&a)
        .arg(&b)
        .output()
        .unwrap());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("delete verified original")
    );
    let output = command(root.path())
        .arg("--yes")
        .arg(&a)
        .arg(&b)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("--allow-cross-volume-delete")
    );
    assert!(b.is_file());
    assert!(!a.exists());
    no_staging(root.path());
}

#[cfg(unix)]
#[test]
fn actual_mv_command_is_undone() {
    let root = TempDir::new().unwrap();
    let a = write(root.path(), "a", "actual mv");
    let b = root.path().join("b");
    assert!(
        std::process::Command::new("mv")
            .arg(&a)
            .arg(&b)
            .status()
            .unwrap()
            .success()
    );
    ok(command(root.path()).arg(a.clone()).arg(b).output().unwrap());
    assert_eq!(fs::read_to_string(a).unwrap(), "actual mv");
}

#[cfg(unix)]
#[test]
fn actual_directory_rename_to_a_trailing_separator_remains_ambiguous() {
    let root = TempDir::new().unwrap();
    let a = root.path().join("a");
    fs::create_dir(&a).unwrap();
    write(&a, "a", "original nested child");
    assert!(
        std::process::Command::new("mv")
            .current_dir(root.path())
            .args(["a", "b/"])
            .status()
            .unwrap()
            .success()
    );
    let output = command(root.path())
        .args(["--yes", "a", "b/"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!a.exists());
    assert_eq!(
        fs::read_to_string(root.path().join("b/a")).unwrap(),
        "original nested child"
    );
    ok(command(root.path())
        .args(["--mode", "rename", "a", "b/"])
        .output()
        .unwrap());
    assert_eq!(
        fs::read_to_string(a.join("a")).unwrap(),
        "original nested child"
    );
}

#[test]
fn emitted_helpers_work_without_path_operands() {
    let root = TempDir::new().unwrap();
    for shell in ["bash", "zsh", "pwsh"] {
        let output = ok(command(root.path())
            .args(["--shell-integration", shell])
            .output()
            .unwrap());
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains("UNMV_CONTEXT")
        );
        assert!(output.stderr.is_empty());
    }
}
