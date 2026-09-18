//! End-to-end CLI tests against a wiremock TypeSafe endpoint. These verify the
//! user-facing contract: stdout stays machine-readable JSON/JSONL, stderr
//! carries diagnostics, and exit codes distinguish success (0), usage or
//! infrastructure errors (1), partial batches (2), and false assertions (3).

use serde_json::{Value, json};
use std::io::Write as _;
use std::process::{Command, Stdio};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const KEY: &str = "test-key";

fn noul_answer(id: &str, v: f64) -> Value {
    json!({
        "model": "jev-latest",
        "answers": {id: {"type": "noul", "noul": v}},
        "usage": {"input_tokens": 42, "output_tokens": 7}
    })
}

struct Jev {
    server: MockServer,
    dir: tempfile::TempDir,
}

impl Jev {
    fn spawn(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_jev"));
        cmd.args(args)
            .env("JEV_API_KEY", KEY)
            .env("JEV_API_BASE", self.server.uri())
            .env("HOME", self.dir.path())
            .env("USERPROFILE", self.dir.path())
            .current_dir(self.dir.path());
        cmd
    }

    async fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> (i32, String, String) {
        let mut child = self
            .spawn(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn jev");
        if let Some(bytes) = stdin {
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(bytes)
                .expect("write stdin");
        } else {
            drop(child.stdin.take());
        }
        let output = child.wait_with_output().expect("wait");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }
}

async fn jev_with_ok_answer(id: &str, v: f64) -> Jev {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(noul_answer(id, v)))
        .mount(&server)
        .await;
    Jev {
        server,
        dir: tempfile::tempdir().unwrap(),
    }
}

fn jsonl(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("each stdout line is JSON"))
        .collect()
}

#[tokio::test]
async fn noul_shortcut_prints_envelope_and_exits_zero() {
    let jev = jev_with_ok_answer("q", 0.93).await;
    let (code, stdout, _) = jev
        .run(
            &[
                "noul",
                "Is this log line an auth failure?",
                "--state",
                "2026-09-18 auth ok",
            ],
            None,
        )
        .await;
    assert_eq!(code, 0);
    let line: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(line["ok"], true);
    assert_eq!(line["answers"]["q"]["noul"], 0.93);
    assert_eq!(line["usage"]["input_tokens"], 42);

    let requests = jev.server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["state"], "2026-09-18 auth ok");
    assert_eq!(body["model"], "jev-latest");
    assert_eq!(body["questions"]["q"]["type"], "noul");
}

#[tokio::test]
async fn noul_reads_state_from_stdin_as_json_when_it_looks_like_json() {
    let jev = jev_with_ok_answer("q", 0.5).await;
    let (code, stdout, _) = jev
        .run(&["noul", "Q?"], Some(br#"{"title":"crash"}"#))
        .await;
    assert_eq!(code, 0);
    let requests = jev.server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["state"]["title"], "crash");
    assert!(stdout.contains("\"ok\":true"));
}

#[tokio::test]
async fn false_assertion_exits_three_with_failed_clause() {
    let jev = jev_with_ok_answer("q", 0.93).await;
    let (code, stdout, _) = jev
        .run(
            &[
                "noul",
                "Q?",
                "--state",
                "x",
                "--assert",
                "answers.q.noul <= 0.5 and usage.input_tokens <= 10",
            ],
            None,
        )
        .await;
    assert_eq!(code, 3);
    let line: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(line["assert"]["passed"], false);
    let failed = line["assert"]["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 2);
    assert_eq!(failed[0]["clause"], "answers.q.noul <= 0.5");
    assert_eq!(failed[0]["expected"], 0.5);
    assert_eq!(failed[0]["actual"], 0.93);
}

#[tokio::test]
async fn unparseable_assertion_fails_before_any_request() {
    let jev = jev_with_ok_answer("q", 0.5).await;
    let (code, stdout, stderr) = jev
        .run(
            &["noul", "Q?", "--state", "x", "--assert", "answers.q.noul ="],
            None,
        )
        .await;
    assert_eq!(code, 1);
    assert!(stdout.is_empty(), "stdout must stay clean: {stdout}");
    assert!(stderr.contains("invalid assertion expression"), "{stderr}");
    assert!(jev.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn assert_path_typo_is_an_error_not_silent_false() {
    let jev = jev_with_ok_answer("q", 0.5).await;
    let (code, stdout, stderr) = jev
        .run(
            &[
                "noul",
                "Q?",
                "--state",
                "x",
                "--assert",
                "answers.typo.noul >= 0.5",
            ],
            None,
        )
        .await;
    assert_eq!(code, 1);
    assert!(stdout.is_empty());
    assert!(stderr.contains("answers.typo.noul"), "{stderr}");
}

#[tokio::test]
async fn ask_applies_params_and_state_override() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(noul_answer("invalid", 0.1)))
        .mount(&server)
        .await;
    let jev = Jev {
        server,
        dir: tempfile::tempdir().unwrap(),
    };
    let template = r#"{
        "description": "triage",
        "params": {"issue": "the issue"},
        "state": {"issue": "{{issue}}"},
        "questions": {
            "invalid": {"type": "noul", "instructions": "Is this report invalid?"}
        },
        "assert": "answers.invalid.noul <= 0.5"
    }"#;
    let template_path = jev.dir.path().join("req.json");
    std::fs::write(&template_path, template).unwrap();
    let issue_path = jev.dir.path().join("issue.json");
    std::fs::write(&issue_path, r#"{"title":"it crashes"}"#).unwrap();

    let (code, stdout, _) = jev
        .run(
            &[
                "ask",
                "req.json",
                "--param",
                &format!("issue=@{}", issue_path.display()),
            ],
            None,
        )
        .await;
    assert_eq!(code, 0, "{stdout}");
    let line: Value = serde_json::from_str(stdout.trim()).unwrap();
    // Preset default assert applies even without --assert.
    assert_eq!(line["assert"]["passed"], true);

    let requests = jev.server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["state"]["issue"]["title"], "it crashes");
    // The template omits model; the CLI must inject the pinned default.
    assert_eq!(body["model"], "jev-latest");
    // Reserved keys never reach the wire.
    assert!(body.get("description").is_none());
    assert!(body.get("assert").is_none());
}

#[tokio::test]
async fn ask_missing_state_is_actionable() {
    let jev = jev_with_ok_answer("q", 0.5).await;
    let req = jev.dir.path().join("nostate.json");
    std::fs::write(
        &req,
        r#"{"questions":{"q":{"type":"noul","instructions":"x"}}}"#,
    )
    .unwrap();
    let (code, stdout, stderr) = jev.run(&["ask", "nostate.json"], None).await;
    assert_eq!(code, 1);
    assert!(stdout.is_empty());
    assert!(stderr.contains("no 'state'"), "{stderr}");
}

#[tokio::test]
async fn auth_failure_hints_login() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
        .mount(&server)
        .await;
    let jev = Jev {
        server,
        dir: tempfile::tempdir().unwrap(),
    };
    let (code, stdout, stderr) = jev.run(&["noul", "Q?", "--state", "x"], None).await;
    assert_eq!(code, 1);
    assert!(stdout.is_empty());
    assert!(stderr.contains("jev login"), "{stderr}");
}

#[tokio::test]
async fn missing_key_points_to_login() {
    let jev = jev_with_ok_answer("q", 0.5).await;
    let mut cmd = jev.spawn(&["noul", "Q?", "--state", "x"]);
    cmd.env_remove("JEV_API_KEY");
    let output = cmd.output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("run jev login"), "{stderr}");
}

#[tokio::test]
async fn map_state_mixed_rows_exit_two_and_jsonl_output() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(|req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            if body["state"]["line"] == 1 {
                ResponseTemplate::new(422).set_body_string("question invalid")
            } else {
                ResponseTemplate::new(200).set_body_json(noul_answer("spam", 0.9))
            }
        })
        .mount(&server)
        .await;
    let jev = Jev {
        server,
        dir: tempfile::tempdir().unwrap(),
    };
    let template = r#"{
        "questions": {"spam": {"type": "noul", "instructions": "Is this spam?"}},
        "assert": "answers.spam.noul <= 0.5"
    }"#;
    let template_path = jev.dir.path().join("t.json");
    std::fs::write(&template_path, template).unwrap();
    let input = jev.dir.path().join("in.jsonl");
    std::fs::write(&input, "{\"line\":0}\n{\"line\":1}\n{\"line\":2}\n").unwrap();
    let out = jev.dir.path().join("out.jsonl");

    let (code, stdout, stderr) = jev
        .run(
            &[
                "map",
                "t.json",
                "--in",
                input.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
                "--concurrency",
                "2",
            ],
            None,
        )
        .await;
    assert_eq!(code, 2, "{stdout}\n{stderr}");
    let lines = jsonl(&stdout);
    assert_eq!(lines.len(), 3);
    let blocked: Vec<&Value> = lines
        .iter()
        .filter(|l| l["assert"]["passed"] == json!(false))
        .collect();
    assert_eq!(blocked.len(), 2, "{stdout}");
    let failed: Vec<&Value> = lines.iter().filter(|l| l["ok"] == json!(false)).collect();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["error"]["kind"], "api");
    // The same three lines land in --out.
    assert_eq!(jsonl(&std::fs::read_to_string(&out).unwrap()).len(), 3);
}

#[tokio::test]
async fn map_resume_skips_ok_rows_on_rerun() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(noul_answer("q", 0.5)))
        // 3 requests in the first run + 2 rerun rows in the resume run.
        .expect(5)
        .mount(&server)
        .await;
    let jev = Jev {
        server,
        dir: tempfile::tempdir().unwrap(),
    };
    let template = r#"{"questions":{"q":{"type":"noul","instructions":"x"}}}"#;
    let template_path = jev.dir.path().join("t.json");
    std::fs::write(&template_path, template).unwrap();
    let input = jev.dir.path().join("in.jsonl");
    std::fs::write(&input, "\"a\"\n\"b\"\n\"c\"\n").unwrap();
    let out = jev.dir.path().join("out.jsonl");

    let (first, _, _) = jev
        .run(
            &[
                "map",
                "t.json",
                "--in",
                input.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
            ],
            None,
        )
        .await;
    assert_eq!(first, 0);
    let first_lines = jsonl(&std::fs::read_to_string(&out).unwrap());
    assert_eq!(first_lines.len(), 3);

    // Simulate: only index 0 completed ok before an interruption.
    let partial: String = first_lines
        .iter()
        .filter(|l| l["index"] == json!(0))
        .map(|l| serde_json::to_string(l).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&out, partial + "\n").unwrap();

    let (second, stdout, _) = jev
        .run(
            &[
                "map",
                "t.json",
                "--in",
                input.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
                "--resume",
            ],
            None,
        )
        .await;
    assert_eq!(second, 0);
    assert_eq!(
        jsonl(&stdout).len(),
        2,
        "only the missing rows rerun: {stdout}"
    );
}

#[tokio::test]
async fn map_each_request_reports_bad_rows_without_killing_the_batch() {
    let jev = jev_with_ok_answer("q", 0.5).await;
    let input = jev.dir.path().join("in.jsonl");
    std::fs::write(
        &input,
        concat!(
            "{\"state\":\"a\",\"questions\":{\"q\":{\"type\":\"noul\",\"instructions\":\"x\"}}}\n",
            "\"not an object\"\n",
            "{\"state\":\"c\",\"questions\":{\"q\":{\"type\":\"noul\",\"instructions\":\"x\"}}}\n",
        ),
    )
    .unwrap();
    let (code, stdout, _) = jev
        .run(
            &["map", "--each", "request", "--in", input.to_str().unwrap()],
            None,
        )
        .await;
    assert_eq!(code, 2);
    let lines = jsonl(&stdout);
    assert_eq!(lines.len(), 3);
    let bad = lines.iter().find(|l| l["ok"] == json!(false)).unwrap();
    assert_eq!(bad["index"], 1);
    assert_eq!(bad["error"]["kind"], "usage");
}

#[tokio::test]
async fn map_each_text_wraps_plain_lines() {
    let jev = jev_with_ok_answer("q", 0.5).await;
    let input = jev.dir.path().join("lines.txt");
    std::fs::write(&input, "plain log line\nanother line\n").unwrap();
    let template = jev.dir.path().join("t.json");
    std::fs::write(
        &template,
        r#"{"questions":{"q":{"type":"noul","instructions":"x"}}}"#,
    )
    .unwrap();
    let (code, stdout, _) = jev
        .run(
            &[
                "map",
                "t.json",
                "--each",
                "text",
                "--in",
                input.to_str().unwrap(),
            ],
            None,
        )
        .await;
    assert_eq!(code, 0);
    assert_eq!(jsonl(&stdout).len(), 2);
    // With concurrency, request arrival order is not deterministic.
    let requests = jev.server.received_requests().await.unwrap();
    let states: Vec<String> = requests
        .iter()
        .map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            body["state"].as_str().unwrap().to_string()
        })
        .collect();
    assert!(states.contains(&"plain log line".to_string()), "{states:?}");
    assert!(states.contains(&"another line".to_string()), "{states:?}");
}

#[tokio::test]
async fn score_shortcut_sends_levels() {
    let jev = jev_with_ok_answer("q", 1.6).await;
    let (code, _, _) = jev
        .run(
            &[
                "score",
                "How stale is the doc?",
                "--level",
                "current",
                "--level",
                "outdated",
                "--state",
                "docs",
            ],
            None,
        )
        .await;
    assert_eq!(code, 0);
    let requests = jev.server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["questions"]["q"]["criteria"][0], "current");
    assert_eq!(body["questions"]["q"]["criteria"][1], "outdated");
}

#[tokio::test]
async fn score_requires_two_levels() {
    let jev = jev_with_ok_answer("q", 0.0).await;
    let (code, _, stderr) = jev
        .run(&["score", "Q?", "--level", "only", "--state", "x"], None)
        .await;
    assert_eq!(code, 1);
    assert!(stderr.contains("at least two --level"), "{stderr}");
}

#[tokio::test]
async fn presets_workflow_list_show_validate() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".jev/presets")).unwrap();
    std::fs::write(
        dir.path().join(".jev/presets/triage.json"),
        r#"{
            "description": "Issue triage",
            "params": {"issue": "the issue text"},
            "state": {"issue": "{{issue}}"},
            "questions": {"invalid": {"type": "noul", "instructions": "Invalid?"}},
            "assert": "answers.invalid.noul <= 0.5"
        }"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join(".jev/presets/broken.json"),
        r#"{"questions": {"a": {"type": "score", "instructions": "x", "criteria": ["only"]}}}"#,
    )
    .unwrap();
    let jev = Jev {
        server: MockServer::start().await,
        dir,
    };

    let (code, stdout, _) = jev.run(&["presets", "list"], None).await;
    assert_eq!(code, 0);
    assert!(stdout.contains("triage  Issue triage"), "{stdout}");
    assert!(stdout.contains("broken"), "{stdout}");

    let (code, stdout, _) = jev.run(&["presets", "show", "triage"], None).await;
    assert_eq!(code, 0);
    assert!(stdout.contains("\"description\": \"Issue triage\""));

    let (code, stdout, _) = jev.run(&["presets", "validate"], None).await;
    assert_eq!(code, 1);
    assert!(stdout.contains("ok triage"), "{stdout}");
    assert!(stdout.contains("error broken"), "{stdout}");
    assert!(stdout.contains("at least two ordered levels"), "{stdout}");

    // No presets dir -> actionable list error.
    let empty = tempfile::tempdir().unwrap();
    let jev = Jev {
        server: MockServer::start().await,
        dir: empty,
    };
    let (code, _, stderr) = jev.run(&["presets", "list"], None).await;
    assert_eq!(code, 1);
    assert!(stderr.contains("no presets found"), "{stderr}");
}

#[tokio::test]
async fn login_needs_a_terminal() {
    let jev = jev_with_ok_answer("q", 0.5).await;
    let (code, _, stderr) = jev.run(&["login"], Some(b"")).await;
    assert_eq!(code, 1);
    assert!(stderr.contains("interactive terminal"), "{stderr}");
}

#[tokio::test]
async fn resume_rejects_stdin_input() {
    let jev = jev_with_ok_answer("q", 0.5).await;
    let (code, _, stderr) = jev
        .run(&["map", "--in", "-", "--out", "o.jsonl", "--resume"], None)
        .await;
    assert_eq!(code, 1);
    assert!(stderr.contains("real --in file"), "{stderr}");
}

#[tokio::test]
async fn version_and_help_exit_zero() {
    let jev = jev_with_ok_answer("q", 0.5).await;
    let (code, stdout, _) = jev.run(&["--version"], None).await;
    assert_eq!(code, 0);
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")));
    let (code, stdout, _) = jev.run(&["--help"], None).await;
    assert_eq!(code, 0);
    assert!(stdout.contains("Usage: jev"), "{stdout}");
}
