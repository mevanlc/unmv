use std::{
    env,
    ffi::OsStr,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, ensure};
use clap::ValueEnum;

use crate::MvKind;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Pwsh,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum DefaultPromptAnswer {
    Guess,
    #[default]
    Cancel,
}

#[derive(Debug)]
pub struct Context {
    pub kind: Option<MvKind>,
    pub verified: bool,
    pub unknown: bool,
    pub description: String,
}

impl Context {
    pub fn detect(override_kind: Option<MvKind>) -> Self {
        let helper = env::var("UNMV_CONTEXT_TYPE").ok();
        let description = env::var("UNMV_CONTEXT_DESCRIPTION").unwrap_or_default();
        let mut result = match helper.as_deref() {
            Some("powershell")
                if env::var("UNMV_CONTEXT_COMMAND").as_deref()
                    == Ok("Microsoft.PowerShell.Management\\Move-Item") =>
            {
                Self {
                    kind: Some(MvKind::Powershell),
                    verified: true,
                    unknown: false,
                    description:
                        "live PowerShell resolution: Microsoft.PowerShell.Management\\Move-Item"
                            .into(),
                }
            }
            Some("external") => {
                let path = env::var_os("UNMV_CONTEXT_COMMAND").map(PathBuf::from);
                let kind = path.as_deref().and_then(identify);
                Self {
                    kind,
                    verified: true,
                    unknown: kind.is_none(),
                    description: format!(
                        "live {} resolution: {description:?}; executable {path:?}",
                        env::var("UNMV_CONTEXT_SHELL").unwrap_or_default()
                    ),
                }
            }
            Some(_) => Self {
                kind: None,
                verified: true,
                unknown: true,
                description: format!("unrecognized live mv resolution: {description:?}"),
            },
            None => {
                let path = find_on_path(OsStr::new("mv"));
                let kind = path.as_deref().and_then(identify);
                Self {
                    kind,
                    verified: false,
                    unknown: path.is_some() && kind.is_none(),
                    description: format!(
                        "live shell context is unverified (shell hint {:?}, PATH mv {path:?}); using literal positional inference",
                        env::var_os("SHELL")
                    ),
                }
            }
        };
        if let Some(kind) = override_kind {
            result.kind = Some(kind);
            result.unknown = false;
            result.description = format!("explicit --mv-kind {kind:?}; {}", result.description);
        }
        result
    }
}

fn find_on_path(name: &OsStr) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        let path = directory.join(name);
        #[cfg(windows)]
        let path = path.with_extension("exe");
        path.is_file().then_some(path)
    })
}

fn identify(path: &Path) -> Option<MvKind> {
    let path = fs::canonicalize(path).ok()?;
    #[cfg(target_os = "macos")]
    if path == Path::new("/bin/mv") {
        return Some(MvKind::Bsd);
    }
    let name = path.file_name()?.to_str()?;
    if !matches!(name, "mv" | "gmv" | "mv.exe" | "gmv.exe") {
        return None;
    }
    let mut magic = [0; 4];
    fs::File::open(&path).ok()?.read_exact(&mut magic).ok()?;
    // Never execute scripts to discover their behavior.
    if !matches!(
        magic,
        [0x7f, b'E', b'L', b'F']
            | [0xcf, 0xfa, 0xed, 0xfe]
            | [0xfe, 0xed, 0xfa, 0xcf]
            | [0xce, 0xfa, 0xed, 0xfe]
            | [0xca, 0xfe, 0xba, 0xbe]
    ) && &magic[..2] != b"MZ"
    {
        return None;
    }
    let mut process = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Read concurrently: an unknown native wrapper can fill stdout or leave
    // an inherited pipe open after it exits. Neither may block inference.
    let stdout = process.stdout.take()?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut output = String::new();
        let result = stdout
            .take(16 * 1024)
            .read_to_string(&mut output)
            .map(|_| output);
        let _ = sender.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(Some(status)) = process.try_wait() {
            if !status.success() {
                return None;
            }
            let output = receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .ok()?
                .ok()?;
            return output
                .starts_with("mv (GNU coreutils)")
                .then_some(MvKind::Gnu);
        }
        if Instant::now() >= deadline {
            let _ = process.kill();
            let _ = process.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn shell_integration(
    shell: Shell,
    default_prompt_answer: DefaultPromptAnswer,
    output: &mut impl Write,
) -> Result<()> {
    let executable = env::current_exe()?;
    let executable = executable
        .to_str()
        .context("shell integration requires a UTF-8 executable path")?;
    let template = match shell {
        Shell::Bash => include_str!("shell/bash.sh"),
        Shell::Zsh => include_str!("shell/zsh.sh"),
        Shell::Pwsh => include_str!("shell/pwsh.ps1"),
    };
    ensure!(
        !executable.contains(['\n', '\r']),
        "executable path cannot contain a newline"
    );
    let quoted = match shell {
        Shell::Pwsh => format!("'{}'", executable.replace('\'', "''")),
        _ => format!("'{}'", executable.replace('\'', "'\\''")),
    };
    let default_prompt_answer = match default_prompt_answer {
        DefaultPromptAnswer::Cancel => "cancel",
        DefaultPromptAnswer::Guess => "guess",
    };
    let source = template
        .replace("@UNMV_DEFAULT_PROMPT_ANSWER@", default_prompt_answer)
        .replace("@UNMV_BIN@", &quoted);
    match output.write_all(source.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
}
