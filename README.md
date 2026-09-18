# Jev CLI

[中文](README.zh-CN.md)

A stateless CLI for typed semantic judgments with the Jev model (TypeSafe System One), plus an Agent Skill for using those judgments in scripts, CI, and agent workflows. Jev does not generate text: you provide a state and typed questions, it returns calibrated probabilities and typed answers. The CLI turns judgments into shell primitives — stable JSON on stdout, diagnostics on stderr, and exit codes you can branch on.

The CLI pins the model to `jev-latest`.

## Install

The skill and CLI are installed separately.

```sh
npx skills add model-clis/jev
```

Windows (preferred):

```powershell
scoop bucket add model-clis https://github.com/model-clis/homebrew-packages
scoop install model-clis/jev
```

Windows without Scoop:

```powershell
irm https://raw.githubusercontent.com/model-clis/jev/main/scripts/install.ps1 | iex
```

Apple Silicon macOS (preferred):

```sh
brew tap model-clis/packages
brew install jev
```

Linux x64 or macOS Apple Silicon without Homebrew:

```sh
curl -fsSL https://raw.githubusercontent.com/model-clis/jev/main/scripts/install.sh | sh
```

Installers fetch only GitHub Release assets, verify SHA-256, and default to `~/.local/bin`. Set `JEV_INSTALL_DIR` to change the destination or a complete tag such as `JEV_VERSION=v2026.918.0` to pin a release. Explicit installer runs may replace an existing binary; the skill never installs or upgrades silently.

## Login and usage

Configure the TypeSafe API key privately in your own terminal—never paste it into a chat:

```sh
jev login
```

The key is stored at `~/model-clis/jev/credentials.json` (mode `600` on Unix). For CI, containers, and other headless automation, set the `JEV_API_KEY` environment variable instead — it takes precedence over the stored credentials. `JEV_API_BASE` overrides the API endpoint (default `https://api.typesafe.ai`) and exists for testing and proxies; everyday use never needs it.

One-question shortcuts (the single answer id is `q`; state comes from `--state` or stdin):

```sh
echo "$LOG" | jev noul "Does this stack trace indicate an out-of-memory kill?" \
    --true "OOM killer or cannot allocate memory" --false "Any other failure"

jev choice "Which team should handle this?" \
    --opt billing="Payments, invoicing, refunds" \
    --opt technical="Bugs, outages, integrations" \
    --opt sales="Pricing, upgrades, accounts" \
    --state @ticket.json

jev score "How current is this documentation?" \
    --level "Matches the API" --level "Minor drift" --level "Badly outdated" \
    --state @README.md
```

Full requests (state + questions) come from a file, stdin, or a preset:

```sh
jev ask request.json
cat request.json | jev ask -
jev ask --preset triage --param issue=@issue.json --param repo=acme/api
```

`@PATH` arguments keep JSON files structured and treat any other file as text.

## Assertions: answers to exit codes

Every judging command accepts `--assert EXPR`. The expression is evaluated against the response envelope (`model`, `answers`, `usage`); when it is false the command still prints the full JSON on stdout and exits `3`. When it cannot be evaluated (unknown path, type mismatch) the command exits `1` — a broken gate must never pass silently.

```text
expr       := or
or         := and { "or" and }
and        := unary { "and" unary }
unary      := "not" unary | "(" expr ")" | comparison
comparison := operand op operand | operand "in" list
operand    := path | number | true | false | "string"
path       := ident { "." ident | "[" "string" "]" }
op         := <= | >= | == | != | < | >
list       := "[" operand { "," operand } "]"
```

```sh
jev ask --preset review-gate --param diff=@pr.diff \
    --assert 'answers.destructive.noul <= 0.5 and answers.touches_creds.noul <= 0.5'

echo "$LOG" | jev noul "OOM kill?" --assert 'answers.q.noul >= 0.8'
jev ask --preset triage --assert 'answers.area.choice in ["billing", "technical"]'
jev ask --preset risk --assert 'answers.risk.probabilities["needs human review"] >= 0.25'
```

Strings use double quotes so the whole expression wraps in single quotes. `not` binds tighter than `and`, which binds tighter than `or`. There is no arithmetic; pipe through `jq` for weighted combinations.

Exit codes: `0` success, `1` usage or infrastructure error (including an unevaluable assertion), `2` partial batch, `3` assertion false, `130` interrupted. `0` never means "the answer was yes" — read `answers` or use `--assert`.

## Presets

Presets are request templates discovered from `./.jev/presets/*.json` (repository; wins on collision) and the user config directory (`~/.config/jev/presets/` on Linux). Committing them makes question wording and thresholds reviewable automation assets.

```json
{
  "description": "Issue triage: severity, routing, invalidity",
  "params": {"issue": "the issue text", "repo": "repository name"},
  "state": {"issue": "{{issue}}", "repo": "{{repo}}"},
  "questions": {
    "severity": {"type": "choice", "instructions": "How severe is this issue?", "criteria": {"blocker": "Data loss or outage", "major": "Broken feature with a workaround", "minor": "Cosmetic or annoying"}},
    "invalid": {"type": "noul", "instructions": "Is this report invalid?", "criteria": {"true": "Spam, duplicate, or unreproducible", "false": "A real report"}}
  },
  "assert": "answers.invalid.noul <= 0.5"
}
```

`description`, `params`, and `assert` are reserved keys stripped before send; `assert` provides the default assertion that an explicit `--assert` replaces. `{{name}}` placeholders must fill an entire field value and are substituted from `--param NAME=VALUE`, `--param NAME=@PATH` (JSON injects as a subtree), or `--param NAME=@-` (stdin). Missing parameters are an error naming the placeholder; unused ones warn.

`jev presets list`, `jev presets show NAME`, and `jev presets validate [NAME|PATH]` work offline — validate catches malformed questions and assertion syntax before anything costs tokens.

## Batches

`jev map` evaluates one template over many inputs concurrently and streams JSONL results in completion order, each line tagged with its input `index`:

```sh
jev map --preset triage --in issues.jsonl --out triaged.jsonl --concurrency 8
jev map --preset classify --in lines.txt --each text
```

- `--each state` (default): each line is any JSON value and becomes the request state.
- `--each text`: each line becomes the state as a plain string.
- `--each request`: each line is a complete request body — generate lines with `jq` when questions must vary per row.

Templates used with `--each state` or `--each text` normally omit `state` entirely, since each line replaces it.

A failed row fails alone (`{"index":N,"ok":false,"error":{"kind":"api"|"network"|"usage","message":"..."}}`) and the run exits `2`. Assertions apply per row; any false row exits `3` (row failures still take precedence with `2`). With `--out FILE --resume`, a rerun skips rows that already completed `ok`. This is checkpointing of your own output file, not caching — the CLI never memoizes results.

Retries and limits: 429/529 and transport errors back off exponentially (500ms doubling, five retries, honoring `retry_after_ms`), each request times out after 30 seconds, and `--concurrency` defaults to 4 (the API allows roughly 20 requests/second; keep the setting at or below 8).

## Output contract

stdout is always stable, single-line JSON (`map` emits one JSON object per line); stderr carries progress, usage summaries, and actionable errors. `--capture-diagnostics` writes CLI diagnostics to a secure temporary file, kept on exit codes other than `0`, `2`, and `3`, and prints `JEV_DIAGNOSTICS=<path>` as the final stderr line.

## Development

Requires Rust 1.89 or newer.

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
npx skills add . --list
sh -n scripts/install.sh
```

## Releases

The automated daily workflow publishes only when `main` has commits not present in the latest release. Versions use Hong Kong dates (`vYYYY.MDD.REV`). Stable assets are produced for Windows x64, Linux x64 (musl), and macOS Apple Silicon, each with a `.sha256` file. There are no nightly or prerelease channels.

Homebrew and Scoop metadata is maintained in [`model-clis/homebrew-packages`](https://github.com/model-clis/homebrew-packages) and reconciled with the latest release daily.

Repository: <https://github.com/model-clis/jev>

## License

MIT
