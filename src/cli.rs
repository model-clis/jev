use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "jev",
    version,
    about = "Typed judgment CLI for the Jev model: state + questions in, calibrated answers and exit codes out",
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
    /// Capture diagnostics in a temporary file and print its path on error.
    #[arg(long, global = true)]
    pub capture_diagnostics: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Verify and store the TypeSafe API key, or set JEV_API_KEY instead.
    Login,
    /// Delete the stored TypeSafe API key.
    Logout,
    /// Ask one yes/no probability question (answer id is "q").
    Noul(NoulArgs),
    /// Pick one option from a set you define (answer id is "q").
    Choice(ChoiceArgs),
    /// Rate the state along ordered levels (answer id is "q").
    Score(ScoreArgs),
    /// Evaluate one request (state + questions) from a file, stdin, or preset.
    Ask(AskArgs),
    /// Evaluate a request template over many JSONL inputs, concurrently.
    Map(MapArgs),
    /// List, show, and validate user presets (no API calls).
    Presets(PresetsArgs),
}

#[derive(Args)]
pub struct NoulArgs {
    pub question: String,
    /// Description of what a yes (value near 1) means.
    #[arg(long = "true", value_name = "DESC")]
    pub yes: Option<String>,
    /// Description of what a no (value near 0) means.
    #[arg(long = "false", value_name = "DESC")]
    pub no: Option<String>,
    #[command(flatten)]
    pub single: SingleArgs,
}

#[derive(Args)]
pub struct ChoiceArgs {
    pub question: String,
    /// Option as KEY=DESCRIPTION; repeat for every option.
    #[arg(long = "opt", value_name = "KEY=DESC", required = true, value_parser = parse_opt)]
    pub opts: Vec<(String, String)>,
    #[command(flatten)]
    pub single: SingleArgs,
}

#[derive(Args)]
pub struct ScoreArgs {
    pub question: String,
    /// Ordered level description, lowest first; repeat at least twice.
    #[arg(long = "level", value_name = "DESC", required = true)]
    pub levels: Vec<String>,
    #[command(flatten)]
    pub single: SingleArgs,
}

#[derive(Args)]
pub struct SingleArgs {
    /// State as literal text, @PATH (JSON file stays structured), or @- for stdin. Defaults to stdin.
    #[arg(long, value_name = "STATE")]
    pub state: Option<String>,
    /// Expression evaluated against the response; exit code 3 when it is false.
    #[arg(long, value_name = "EXPR")]
    pub assert: Option<String>,
}

#[derive(Args)]
pub struct AskArgs {
    /// Request file path, or - for stdin. Required unless --preset is given.
    #[arg(value_name = "PATH", conflicts_with = "preset")]
    pub request: Option<String>,
    /// Load the request from a preset by name.
    #[arg(long, value_name = "NAME")]
    pub preset: Option<String>,
    /// Set or override the request state.
    #[arg(long, value_name = "STATE")]
    pub state: Option<String>,
    /// Template parameter NAME=VALUE, NAME=@PATH (JSON stays structured), or NAME=@- for stdin; repeatable.
    #[arg(long = "param", value_name = "K=V")]
    pub params: Vec<String>,
    /// Expression evaluated against the response; overrides the preset default; exit code 3 when false.
    #[arg(long, value_name = "EXPR")]
    pub assert: Option<String>,
}

#[derive(Args)]
pub struct MapArgs {
    /// Request template path, or - for stdin. Required for --each state|text unless --preset is given.
    #[arg(value_name = "PATH", conflicts_with = "preset")]
    pub request: Option<String>,
    /// Load the request template from a preset by name.
    #[arg(long, value_name = "NAME")]
    pub preset: Option<String>,
    /// Input JSONL file, or - for stdin. Required.
    #[arg(long = "in", value_name = "PATH")]
    pub input: String,
    /// How each input line is interpreted.
    #[arg(long, value_enum, default_value_t = Each::State)]
    pub each: Each,
    /// Parallel in-flight requests. The API allows roughly 20 requests/second.
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..))]
    pub concurrency: u32,
    /// Write JSONL results to PATH as they complete.
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,
    /// With --out, skip indices already completed ok in that file.
    #[arg(long, requires = "out")]
    pub resume: bool,
    /// Template parameter NAME=VALUE or NAME=@PATH; repeatable.
    #[arg(long = "param", value_name = "K=V")]
    pub params: Vec<String>,
    /// Expression evaluated against each row; overrides the preset default; exit code 3 when any row is false.
    #[arg(long, value_name = "EXPR")]
    pub assert: Option<String>,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Each {
    /// Each line becomes the request state (any JSON value).
    State,
    /// Each line is a complete request body.
    Request,
    /// Each line becomes the state as a plain JSON string.
    Text,
}

#[derive(Args)]
pub struct PresetsArgs {
    #[command(subcommand)]
    pub command: PresetsCommand,
}

#[derive(Subcommand)]
pub enum PresetsCommand {
    /// List discovered presets with their descriptions.
    List,
    /// Print one preset file as pretty JSON.
    Show { name: String },
    /// Validate a preset or template file, or every discovered preset.
    Validate { target: Option<String> },
}

fn parse_opt(spec: &str) -> Result<(String, String), String> {
    let (key, value) = spec
        .split_once('=')
        .ok_or_else(|| format!("expected KEY=DESCRIPTION, got '{spec}'"))?;
    if key.is_empty() || value.is_empty() {
        return Err(format!("expected KEY=DESCRIPTION, got '{spec}'"));
    }
    Ok((key.to_string(), value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_parses_representative_invocations() {
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from([
            "jev",
            "noul",
            "Is this urgent?",
            "--state",
            "@issue.json",
            "--assert",
            "answers.q.noul >= 0.8",
        ])
        .unwrap();
        let Command::Noul(args) = cli.command else {
            panic!("expected noul");
        };
        assert_eq!(args.question, "Is this urgent?");
        assert_eq!(args.single.state.as_deref(), Some("@issue.json"));

        let cli = Cli::try_parse_from([
            "jev",
            "choice",
            "Route this.",
            "--opt",
            "billing=Money",
            "--opt",
            "tech=Bugs",
        ])
        .unwrap();
        let Command::Choice(args) = cli.command else {
            panic!("expected choice");
        };
        assert_eq!(args.opts.len(), 2);

        let cli = Cli::try_parse_from([
            "jev",
            "map",
            "--preset",
            "triage",
            "--in",
            "items.jsonl",
            "--each",
            "text",
            "--concurrency",
            "8",
            "--out",
            "r.jsonl",
            "--resume",
        ])
        .unwrap();
        let Command::Map(args) = cli.command else {
            panic!("expected map");
        };
        assert_eq!(args.each, Each::Text);
        assert_eq!(args.concurrency, 8);
        assert!(args.resume);
    }

    #[test]
    fn map_requires_in_and_resume_requires_out() {
        assert!(Cli::try_parse_from(["jev", "map"]).is_err());
        assert!(Cli::try_parse_from(["jev", "map", "--in", "x", "--resume"]).is_err());
        assert!(Cli::try_parse_from(["jev", "map", "--in", "x", "--out", "o", "--resume"]).is_ok());
    }

    #[test]
    fn opt_parser_rejects_missing_pieces() {
        assert!(parse_opt("billing=Money").is_ok());
        assert!(parse_opt("billing").is_err());
        assert!(parse_opt("=x").is_err());
        assert!(parse_opt("x=").is_err());
    }
}
