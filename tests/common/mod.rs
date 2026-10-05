#![allow(dead_code)]
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;
use unmv::{Cancellation, ExecutionOptions, Inference, Mode, Plan};

pub fn plan(paths: &[PathBuf], mode: Mode) -> Plan {
    match unmv::infer(paths, Some(mode), None).unwrap() {
        Inference::Certain(plan) => plan,
        Inference::Ambiguous(_) => panic!("explicit mode was ambiguous"),
    }
}

pub fn run(plan: &Plan) -> anyhow::Result<()> {
    unmv::execute(
        plan,
        ExecutionOptions::default(),
        &Cancellation::default(),
        |_| Ok(true),
    )
}

pub fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_unmv"));
    command.current_dir(root);
    for name in [
        "UNMV_CONTEXT_SHELL",
        "UNMV_CONTEXT_TYPE",
        "UNMV_CONTEXT_COMMAND",
        "UNMV_CONTEXT_DESCRIPTION",
        "UNMV_DEFAULT_PROMPT_ANSWER",
    ] {
        command.env_remove(name);
    }
    command
}

pub fn ok(output: Output) -> Output {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

pub fn write(root: &Path, name: &str, value: &str) -> PathBuf {
    let path = root.join(name);
    fs::write(&path, value).unwrap();
    path
}

pub fn cross_volumes() -> Option<(TempDir, TempDir)> {
    let root = TempDir::new().unwrap();
    let other = std::env::var_os("UNMV_TEST_VOLUME")
        .map(PathBuf::from)
        .or_else(|| cfg!(target_os = "linux").then(|| PathBuf::from("/dev/shm")));
    let Some(other) = other else {
        assert!(
            std::env::var_os("UNMV_REQUIRE_CROSS_VOLUME_TESTS").is_none(),
            "UNMV_TEST_VOLUME must name a second filesystem"
        );
        eprintln!("cross-volume test unavailable: set UNMV_TEST_VOLUME to a second filesystem");
        return None;
    };
    let moved = TempDir::new_in(other).unwrap();
    let from = write(moved.path(), "b", "volume probe");
    let to = root.path().join("a");
    let probe = plan(&[to, from.clone()], Mode::Rename);
    assert!(
        probe.is_cross_volume(),
        "UNMV_TEST_VOLUME must be a different mount/volume"
    );
    fs::remove_file(from).unwrap();
    Some((root, moved))
}

pub fn copy_options() -> ExecutionOptions {
    ExecutionOptions {
        allow_cross_volume_delete: true,
        allow_metadata_loss: cfg!(windows),
    }
}

#[cfg(unix)]
pub fn symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}
#[cfg(windows)]
pub fn symlink(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_file(target, link).unwrap();
}

pub fn no_staging(root: &Path) {
    assert!(fs::read_dir(root).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".unmv-")
    }));
}
