mod common;
use common::*;
#[cfg(unix)]
use std::process::Stdio;
use std::{fs, process::Command};
use tempfile::TempDir;

#[cfg(unix)]
fn shell_test(shell: &str, setup: &str, invocation: &str) -> Option<std::process::Output> {
    shell_test_with_default(shell, None, setup, invocation)
}

#[cfg(unix)]
fn shell_test_with_default(
    shell: &str,
    default_prompt_answer: Option<&str>,
    setup: &str,
    invocation: &str,
) -> Option<std::process::Output> {
    if Command::new(shell)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        return None;
    }
    let root = TempDir::new().unwrap();
    write(root.path(), "b space", "payload");
    let mut generator = command(root.path());
    generator.args(["--shell-integration", shell]);
    if let Some(mode) = default_prompt_answer {
        generator.args(["--default-prompt-answer", mode]);
    }
    let helper = ok(generator.output().unwrap());
    let helper_path = root.path().join("helper");
    fs::write(&helper_path, helper.stdout).unwrap();
    let options = if shell == "bash" {
        "shopt -s expand_aliases"
    } else {
        ""
    };
    let script = format!("{options}\nsource ./helper\n{setup}\n{invocation}\n");
    let output = Command::new(shell)
        .args(["-f", "-c"])
        .arg(script)
        .current_dir(root.path())
        .env_remove("UNMV_CONTEXT_TYPE")
        .env_remove("UNMV_DEFAULT_PROMPT_ANSWER")
        .env_remove("BASH_ENV")
        .output()
        .unwrap();
    if output.status.success() {
        assert_eq!(
            fs::read_to_string(root.path().join("a space")).unwrap(),
            "payload"
        );
        assert!(!root.path().join("b space").exists());
    } else {
        assert!(!root.path().join("a space").exists());
        assert_eq!(
            fs::read_to_string(root.path().join("b space")).unwrap(),
            "payload"
        );
    }
    Some(output)
}

#[cfg(unix)]
#[test]
fn bash_and_zsh_helper_defaults_allow_one_off_modes() {
    for shell in ["bash", "zsh"] {
        for (default_prompt_answer, invocation) in [
            ("guess", "unmv 'a space' 'b space'"),
            ("cancel", "unmv 'a space' 'b space'"),
            (
                "cancel",
                "unmv --default-prompt-answer guess 'a space' 'b space'",
            ),
            (
                "cancel",
                "unmv --default-prompt-answer=guess 'a space' 'b space'",
            ),
            ("cancel", "unmv --mode rename 'a space' 'b space'"),
            ("guess", "unmv --mode rename 'a space' 'b space'"),
        ] {
            let Some(output) =
                shell_test_with_default(shell, Some(default_prompt_answer), "", invocation)
            else {
                continue;
            };
            assert!(
                output.status.success(),
                "{shell} default {default_prompt_answer}, {invocation}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let invocation = format!(
            "unmv --shell-integration {shell} --default-prompt-answer guess > ./new-helper\nsource ./new-helper\nunmv 'a space' 'b space'"
        );
        if let Some(output) = shell_test_with_default(shell, Some("cancel"), "", &invocation) {
            ok(output);
        }
        if let Some(output) = shell_test_with_default(
            shell,
            Some("cancel"),
            "export UNMV_DEFAULT_PROMPT_ANSWER=parent",
            "unmv --default-prompt-answer guess 'a space' 'b space'\n[[ $UNMV_DEFAULT_PROMPT_ANSWER == parent ]]",
        ) {
            ok(output);
        }
    }
}

#[cfg(unix)]
#[test]
fn bash_and_zsh_helpers_resolve_live_aliases_and_preserve_arguments() {
    for shell in ["bash", "zsh"] {
        let Some(output) = shell_test(shell, "alias mv='mv -i'", "unmv 'a space' 'b space'") else {
            continue;
        };
        ok(output);
        let Some(output) = shell_test(shell, "alias mv='mv -T'", "unmv 'a space' 'b space'") else {
            continue;
        };
        assert_eq!(output.status.code(), Some(1));
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("--mv-kind")
        );
    }
}

#[cfg(unix)]
#[test]
fn custom_functions_are_reported_without_being_executed() {
    for shell in ["bash", "zsh"] {
        let Some(output) = shell_test(
            shell,
            "function mv() { printf 'FUNCTION EXECUTED'; }",
            "unmv 'a space' 'b space'",
        ) else {
            continue;
        };
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("FUNCTION EXECUTED"));
        assert!(stderr.contains("--mv-kind"));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_helpers_recognize_gnu_mv_aliases() {
    if Command::new("gmv").arg("--version").output().is_err() {
        eprintln!("gmv unavailable");
        return;
    }
    for shell in ["bash", "zsh"] {
        let output = shell_test(shell, "alias mv=gmv", "unmv 'a space' 'b space'").unwrap();
        ok(output);
    }
}

#[test]
fn powershell_helper_resolves_cmdlets_arrays_and_shadowing() {
    let mut ran = false;
    for shell in ["pwsh", "powershell"] {
        if shell == "powershell" && !cfg!(windows) {
            continue;
        }
        if !Command::new(shell)
            .args(["-NoProfile", "-Command", "exit 0"])
            .output()
            .is_ok_and(|output| output.status.success())
        {
            continue;
        }
        ran = true;
        let root = TempDir::new().unwrap();
        let helper = ok(command(root.path())
            .args(["--shell-integration", "pwsh"])
            .output()
            .unwrap());
        fs::write(root.path().join("helper.ps1"), helper.stdout).unwrap();
        let script_path = root.path().join("test.ps1");
        fs::write(&script_path, include_str!("scripts/powershell.ps1")).unwrap();
        ok(Command::new(shell)
            .args(["-NoLogo", "-NoProfile", "-File"])
            .arg(script_path)
            .arg(root.path())
            .output()
            .unwrap());
    }
    if !ran {
        eprintln!("PowerShell runtime unavailable; add pwsh to PATH to exercise this helper");
    }
}

#[test]
fn powershell_helper_defaults_allow_one_off_modes() {
    let mut ran = false;
    for shell in ["pwsh", "powershell"] {
        if shell == "powershell" && !cfg!(windows) {
            continue;
        }
        if !Command::new(shell)
            .args(["-NoProfile", "-Command", "exit 0"])
            .output()
            .is_ok_and(|output| output.status.success())
        {
            continue;
        }
        ran = true;
        for (default_prompt_answer, invocation) in [
            ("guess", "unmv 'a space' 'b space'"),
            ("cancel", "unmv 'a space' 'b space'"),
            (
                "cancel",
                "unmv --default-prompt-answer guess 'a space' 'b space'",
            ),
            (
                "cancel",
                "unmv --default-prompt-answer=guess 'a space' 'b space'",
            ),
            ("cancel", "unmv --mode rename 'a space' 'b space'"),
            ("guess", "unmv --mode rename 'a space' 'b space'"),
        ] {
            let root = TempDir::new().unwrap();
            write(root.path(), "b space", "payload");
            let helper = ok(command(root.path())
                .args([
                    "--shell-integration",
                    "pwsh",
                    "--default-prompt-answer",
                    default_prompt_answer,
                ])
                .output()
                .unwrap());
            fs::write(root.path().join("helper.ps1"), helper.stdout).unwrap();
            let script = format!(
                "$ErrorActionPreference = 'Stop'\nSet-Alias -Name mv -Value Move-Item -Option AllScope\n. ./helper.ps1\n{invocation}\nexit $LASTEXITCODE"
            );
            let output = Command::new(shell)
                .args(["-NoLogo", "-NoProfile", "-Command", &script])
                .current_dir(root.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{shell} default {default_prompt_answer}, {invocation}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                fs::read_to_string(root.path().join("a space")).unwrap(),
                "payload"
            );
            assert!(!root.path().join("b space").exists());
        }
    }
    if !ran {
        eprintln!("PowerShell runtime unavailable; add pwsh to PATH to exercise this helper");
    }
}
