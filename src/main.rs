mod api;
mod assert;
mod cli;
mod config;
mod diagnostics;
mod presets;
mod request;
mod run;

use anyhow::{Result, bail};
use clap::{Parser, error::ErrorKind};
use cli::{AskArgs, Cli, Command, Each, MapArgs, PresetsCommand};
use std::{
    ffi::{OsStr, OsString},
    io::Write as _,
    process::ExitCode,
};

#[tokio::main]
async fn main() -> ExitCode {
    diagnostics::init(capture_diagnostics_requested_from(std::env::args_os()));
    let task = run();
    tokio::pin!(task);
    let exit_code = tokio::select! {
        result = &mut task => match result {
            Ok(code) => code,
            Err(e) => { diagnostics::log(format_args!("Error: {e:#}")); 1 }
        },
        _ = tokio::signal::ctrl_c() => { diagnostics::log(format_args!("Interrupted")); 130 }
    };
    let _ = std::io::stdout().flush();
    diagnostics::finish(exit_code);
    ExitCode::from(exit_code)
}

fn capture_diagnostics_requested_from(args: impl IntoIterator<Item = OsString>) -> bool {
    args.into_iter()
        .skip(1)
        .take_while(|arg| arg.as_os_str() != OsStr::new("--"))
        .any(|arg| arg.as_os_str() == OsStr::new("--capture-diagnostics"))
}

async fn run() -> Result<u8> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {
            e.print()?;
            return Ok(0);
        }
        Err(e) => {
            // Arg errors are user-facing, not diagnostics.
            eprint!("{e}");
            return Ok(1);
        }
    };
    let _ = cli.capture_diagnostics;
    match cli.command {
        Command::Login => login().await,
        Command::Logout => {
            config::logout()?;
            Ok(0)
        }
        Command::Noul(args) => {
            let state = request::load_state_value(args.single.state.as_deref())?;
            let body = request::shortcut_noul(
                &args.question,
                args.yes.as_deref(),
                args.no.as_deref(),
                state,
            );
            execute_single(body, args.single.assert.as_deref()).await
        }
        Command::Choice(args) => {
            let state = request::load_state_value(args.single.state.as_deref())?;
            let body = request::shortcut_choice(&args.question, &args.opts, state);
            execute_single(body, args.single.assert.as_deref()).await
        }
        Command::Score(args) => {
            if args.levels.len() < 2 {
                bail!("score needs at least two --level descriptions, lowest first");
            }
            let state = request::load_state_value(args.single.state.as_deref())?;
            let body = request::shortcut_score(&args.question, &args.levels, state);
            execute_single(body, args.single.assert.as_deref()).await
        }
        Command::Ask(args) => ask(args).await,
        Command::Map(args) => map(args).await,
        Command::Presets(args) => match args.command {
            PresetsCommand::List => {
                let found = presets::list()?;
                if found.is_empty() {
                    bail!(
                        "no presets found; create .jev/presets/NAME.json or {}",
                        user_presets_hint()
                    );
                }
                for f in found {
                    match f.description {
                        Some(d) => println!("{}  {}  [{}]", f.name, d, f.path.display()),
                        None => println!("{}  [{}]", f.name, f.path.display()),
                    }
                }
                Ok(0)
            }
            PresetsCommand::Show { name } => {
                print!("{}", presets::show(&name)?);
                Ok(0)
            }
            PresetsCommand::Validate { target } => {
                let invalid = presets::validate(target.as_deref())?;
                Ok(if invalid > 0 { 1 } else { 0 })
            }
        },
    }
}

fn user_presets_hint() -> String {
    dirs::config_dir()
        .map(|d| d.join("jev/presets/NAME.json").display().to_string())
        .unwrap_or_else(|| "the user presets directory".to_string())
}

async fn login() -> Result<u8> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!("login requires an interactive terminal");
    }
    let key = rpassword::prompt_password("TypeSafe API key: ")?;
    if key.trim().is_empty() {
        bail!("API key must not be empty");
    }
    let base = std::env::var("JEV_API_BASE").unwrap_or_else(|_| api::DEFAULT_BASE.to_string());
    api::Client::new(key.clone(), &base)?
        .verify()
        .await
        .map_err(|e| e.context("login verification failed"))?;
    config::save_key(&key)?;
    diagnostics::log(format_args!("Login successful"));
    Ok(0)
}

fn client() -> Result<api::Client> {
    let key = config::load_key()?;
    let base = std::env::var("JEV_API_BASE").unwrap_or_else(|_| api::DEFAULT_BASE.to_string());
    api::Client::new(key, &base)
}

async fn execute_single(body: serde_json::Value, assert_src: Option<&str>) -> Result<u8> {
    let expr = parse_assert(assert_src)?;
    request::validate_ready(&body)?;
    let outcome = run::single(&client()?, &body, expr.as_ref()).await?;
    println!("{}", outcome.line);
    Ok(outcome.exit_code)
}

fn parse_assert(src: Option<&str>) -> Result<Option<assert::Expr>> {
    match src {
        Some(src) => Ok(Some(assert::parse(src)?)),
        None => Ok(None),
    }
}

async fn ask(args: AskArgs) -> Result<u8> {
    let mut template = load_template(args.request.as_deref(), args.preset.as_deref())?;
    let params = parse_params(&args.params)?;
    request::apply_params(&mut template.body, &params)?;
    if let Some(state) = &args.state {
        template.body["state"] = request::load_state_value(Some(state))?;
    }
    let assert_src = args
        .assert
        .as_deref()
        .or(template.default_assert.as_deref());
    execute_single(template.body, assert_src).await
}

async fn map(args: MapArgs) -> Result<u8> {
    if args.resume && args.input == "-" {
        bail!("--resume needs a real --in file, not stdin (the input is re-read)");
    }
    let needs_template = args.each != Each::Request;
    let template = match (args.request.as_deref(), args.preset.as_deref()) {
        (Some(_), Some(_)) => unreachable!("clap enforces the conflict"),
        (request, preset) if request.is_some() || preset.is_some() || needs_template => {
            Some(load_template(request, preset)?)
        }
        _ => None,
    };
    let assert_src = args
        .assert
        .as_deref()
        .or(template.as_ref().and_then(|t| t.default_assert.as_deref()));
    let expr = parse_assert(assert_src)?;
    let base = match &template {
        Some(template) => {
            let mut body = template.body.clone();
            let params = parse_params(&args.params)?;
            request::apply_params(&mut body, &params)?;
            request::validate_questions(&body, "template")?;
            Some(body)
        }
        None => {
            if !args.params.is_empty() {
                bail!("--param needs a template; with --each request each line is a full body");
            }
            None
        }
    };

    let lines = read_input_lines(&args.input).await?;
    let mut items = Vec::new();
    let mut pre_errors = Vec::new();
    for (index, line) in lines.into_iter().enumerate() {
        match build_row(&base, args.each, &line) {
            Ok(body) => items.push((index, body)),
            Err(message) => {
                pre_errors.push((index, format!("input line {}: {message}", index + 1)))
            }
        }
    }

    let started = std::time::Instant::now();
    let outcome = run::batch(
        client()?,
        items,
        pre_errors,
        expr.as_ref(),
        args.concurrency,
        args.out.as_deref(),
        args.resume,
    )
    .await?;
    eprintln!();
    eprintln!(
        "{}: {} ok, {} failed, {} blocked by assert | Tokens: in {}, out {} | Elapsed: {}s",
        outcome.done,
        outcome.ok,
        outcome.failed,
        outcome.assert_blocked,
        outcome.input_tokens,
        outcome.output_tokens,
        started.elapsed().as_secs()
    );
    Ok(outcome.exit_code)
}

fn build_row(
    base: &Option<serde_json::Value>,
    each: Each,
    line: &str,
) -> Result<serde_json::Value> {
    match each {
        Each::Request => {
            let value: serde_json::Value =
                serde_json::from_str(line).map_err(|e| anyhow::anyhow!("not valid JSON: {e}"))?;
            if !value.is_object() {
                bail!("expected a JSON object request body");
            }
            request::validate_ready(&value)?;
            Ok(value)
        }
        Each::State => {
            let mut body = base.clone().ok_or_else(|| {
                anyhow::anyhow!("--each state needs a template (PATH, -, or --preset)")
            })?;
            let state: serde_json::Value = serde_json::from_str(line).map_err(|e| {
                anyhow::anyhow!("not valid JSON (use --each text for plain lines): {e}")
            })?;
            body["state"] = state;
            request::validate_ready(&body)?;
            Ok(body)
        }
        Each::Text => {
            let mut body = base.clone().ok_or_else(|| {
                anyhow::anyhow!("--each text needs a template (PATH, -, or --preset)")
            })?;
            body["state"] = serde_json::Value::String(line.to_string());
            request::validate_ready(&body)?;
            Ok(body)
        }
    }
}

async fn read_input_lines(path: &str) -> Result<Vec<String>> {
    let raw = if path == "-" {
        request::read_stdin("input lines")?
    } else {
        std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read --in {path}: {e}"))?
    };
    Ok(raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

fn load_template(request: Option<&str>, preset: Option<&str>) -> Result<request::Template> {
    if let Some(name) = preset {
        return presets::load_by_name(name);
    }
    match request {
        Some("-") => request::parse_template(&request::read_stdin("request")?, "stdin"),
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("failed to read request {path}: {e}"))?;
            request::parse_template(&text, path)
        }
        None => request::parse_template(&request::read_stdin("request")?, "stdin"),
    }
}

fn parse_params(specs: &[String]) -> Result<Vec<(String, serde_json::Value)>> {
    specs.iter().map(|s| request::parse_param(s)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_flag_scan_stops_at_argument_delimiter() {
        assert!(capture_diagnostics_requested_from([
            OsString::from("jev"),
            OsString::from("ask"),
            OsString::from("--capture-diagnostics"),
        ]));
        assert!(!capture_diagnostics_requested_from([
            OsString::from("jev"),
            OsString::from("--"),
            OsString::from("--capture-diagnostics"),
        ]));
    }
}
