use anyhow::{Context as _, Result, bail, ensure};
use clap::{CommandFactory, Parser, ValueEnum, error::ErrorKind};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    process::ExitCode,
};
use unmv::{
    Cancellation, Context, DefaultPromptAnswer, ExecutionOptions, Inference, Mode, MvKind, Plan,
    Shell, Suggestion, Summary,
};

#[derive(Parser)]
#[command(
    version,
    about,
    override_usage = "unmv [OPTIONS] <SOURCE>... <DESTINATION>\n       unmv --shell-integration <SHELL> [--default-prompt-answer <guess|cancel>]",
    after_help = "Use the original mv operands. No history log is required.\nUnknown wrappers require explicit mv semantics; ambiguous undos require an interactive choice.\nOriginal mv flags, wildcard reconstruction, and non-filesystem providers are outside v1."
)]
struct Cli {
    /// Preview inferred moves and required authorizations without modifying files
    #[arg(long)]
    dry_run: bool,
    /// Choose whether mv renamed its source or moved it into a directory
    #[arg(long, value_enum)]
    mode: Option<Mode>,
    /// Explicitly assume the original mv implementation (overrides shell inspection)
    #[arg(long, value_enum)]
    mv_kind: Option<MvKind>,
    /// Authorize verified copying followed by PERMANENT deletion across volumes
    #[arg(long)]
    allow_cross_volume_delete: bool,
    /// Permit specifically reported metadata and hard-link preservation losses
    #[arg(long)]
    allow_metadata_loss: bool,
    /// Skip execution confirmation; still requires risk flags and explicit ambiguity resolution
    #[arg(long)]
    yes: bool,
    /// Print an optional helper that inspects the invoking shell's live mv resolution
    #[arg(long, value_enum, conflicts_with_all = ["dry_run", "mode", "mv_kind", "allow_cross_volume_delete", "allow_metadata_loss", "yes", "paths"])]
    shell_integration: Option<Shell>,
    /// Default answer when Enter is pressed at an ambiguous undo prompt (default: cancel)
    #[arg(long, value_enum)]
    default_prompt_answer: Option<DefaultPromptAnswer>,
    /// Original source operands followed by the original destination
    #[arg(value_name = "PATH", num_args = 2.., required_unless_present = "shell_integration")]
    paths: Vec<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.mode == Some(Mode::Rename) && cli.paths.len() != 2 {
        Cli::command()
            .error(
                ErrorKind::ArgumentConflict,
                "--mode rename requires exactly one source and one destination",
            )
            .exit();
    }
    let cancel = Cancellation::default();
    let handler = cancel.clone();
    if let Err(error) = ctrlc::set_handler(move || handler.cancel()) {
        eprintln!("unmv: cannot install cancellation handler: {error}");
        return ExitCode::FAILURE;
    }
    match run(cli, &cancel) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("unmv: {error:#}");
            if cancel.is_cancelled() {
                ExitCode::from(130)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

fn run(cli: Cli, cancel: &Cancellation) -> Result<()> {
    if let Some(shell) = cli.shell_integration {
        return unmv::shell_integration(
            shell,
            cli.default_prompt_answer.unwrap_or_default(),
            &mut io::stdout().lock(),
        );
    }
    let prompt_answer = if let Some(answer) = cli.default_prompt_answer {
        answer
    } else {
        let value =
            std::env::var_os("UNMV_DEFAULT_PROMPT_ANSWER").unwrap_or_else(|| "cancel".into());
        let value = value
            .to_str()
            .context("UNMV_DEFAULT_PROMPT_ANSWER must be UTF-8")?;
        DefaultPromptAnswer::from_str(value, false)
            .map_err(anyhow::Error::msg)
            .context("invalid UNMV_DEFAULT_PROMPT_ANSWER")?
    };
    let context = Context::detect(cli.mv_kind);
    if !context.verified || context.unknown || cli.mv_kind.is_some() {
        eprintln!("unmv: {}", context.description);
    }
    ensure!(
        !context.unknown || cli.dry_run,
        "mv behavior is unrecognized; supply --mv-kind bsd|gnu|powershell after reviewing the detected definition"
    );
    let inference = unmv::infer(&cli.paths, cli.mode, context.kind)?;
    let suggestion = if prompt_answer == DefaultPromptAnswer::Guess {
        inference.suggestion()
    } else {
        None
    };
    let plan = match inference {
        Inference::Certain(plan) => plan,
        Inference::Ambiguous(plans) => {
            eprintln!("unmv: more than one undo is plausible:");
            for (index, plan) in plans.iter().enumerate() {
                let marker = if suggestion.is_some_and(|suggestion| suggestion.index == index) {
                    " (suggested default)"
                } else {
                    ""
                };
                eprintln!(
                    "  {}. {:?}{marker}\n{}",
                    index + 1,
                    plan.mode,
                    plan.preview()
                );
            }
            if let Some(suggestion) = suggestion {
                eprintln!("unmv: guess: {}", suggestion.reason);
            }
            ensure!(
                !cli.dry_run,
                "choose --mode rename or --mode into-dir to preview one interpretation"
            );
            ensure!(
                !cli.yes,
                "--yes cannot resolve ambiguity; choose --mode rename or --mode into-dir"
            );
            choose(plans, suggestion, cancel)?
        }
    };
    if cli.dry_run {
        write_stdout(plan.preview().as_bytes())?;
        if plan.is_cross_volume() {
            eprintln!(
                "unmv: cross-volume metadata compatibility will be verified in staging before publication; --allow-cross-volume-delete is required for execution"
            );
        }
        return Ok(());
    }
    if plan.is_cross_volume() && !cli.yes {
        ensure!(
            interactive(),
            "cross-volume execution needs interactive confirmation or --yes, as well as --allow-cross-volume-delete"
        );
    }
    unmv::execute(
        &plan,
        ExecutionOptions {
            allow_cross_volume_delete: cli.allow_cross_volume_delete,
            allow_metadata_loss: cli.allow_metadata_loss,
        },
        cancel,
        |summary| confirm(summary, cli.yes, cancel),
    )
}

fn interactive() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

fn read_answer(prompt: &str, cancel: &Cancellation) -> Result<String> {
    ensure!(
        interactive(),
        "interactive choice requires a terminal; choose --mode rename or --mode into-dir"
    );
    eprint!("{prompt}");
    io::stderr().flush()?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut answer = String::new();
        let result = io::stdin()
            .read_line(&mut answer)
            .map(|read| (read, answer));
        let _ = sender.send(result);
    });
    loop {
        cancel.check()?;
        match receiver.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(answer) => {
                let (read, answer) = answer?;
                ensure!(read != 0, "operation cancelled; no files changed");
                return Ok(answer.trim().to_owned());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn choose(
    mut plans: Vec<Plan>,
    suggestion: Option<Suggestion>,
    cancel: &Cancellation,
) -> Result<Plan> {
    let prompt = if let Some(suggestion) = suggestion {
        format!(
            "Choose 1 or 2 [{}] (Enter accepts guess; q cancels): ",
            suggestion.index + 1
        )
    } else {
        "Choose 1 or 2 (Enter cancels): ".to_owned()
    };
    let answer = read_answer(&prompt, cancel)?;
    let chosen = match answer.as_str() {
        "1" => Some(0),
        "2" => Some(1),
        "" => suggestion.map(|suggestion| suggestion.index),
        _ => None,
    };
    match chosen {
        Some(index) => Ok(plans.remove(index)),
        None => bail!("operation cancelled; no files changed"),
    }
}

fn confirm(summary: &Summary, yes: bool, cancel: &Cancellation) -> Result<bool> {
    eprintln!(
        "unmv: verified cross-volume restore, {} entries, {} logical bytes:\n{}",
        summary.entries, summary.logical_bytes, summary.preview
    );
    for loss in &summary.losses {
        eprintln!("unmv: authorized metadata loss: {loss}");
    }
    eprintln!(
        "unmv: copied originals will be permanently deleted; source changes or failures can leave partial restores and retained copies"
    );
    if yes {
        return Ok(true);
    }
    Ok(read_answer(
        "Publish these restores and delete verified originals? [y/N] ",
        cancel,
    )?
    .eq_ignore_ascii_case("y"))
}

fn write_stdout(value: &[u8]) -> io::Result<()> {
    match io::stdout().lock().write_all(value) {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}
