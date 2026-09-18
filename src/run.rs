//! Execution: one request -> one JSON envelope line, or a concurrent batch of
//! requests -> JSONL lines in completion order with per-row outcomes.

use crate::api::{ApiError, Client, Evaluation};
use crate::assert::{Expr, Outcome as AssertOutcome};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    io::{BufWriter, Write as _},
    path::Path,
    sync::Arc,
    time::Instant,
};
use tokio::{sync::Semaphore, task::JoinSet};

pub struct SingleOutcome {
    pub exit_code: u8,
    pub line: String,
}

pub async fn single(
    client: &Client,
    body: &Value,
    assert_expr: Option<&Expr>,
) -> Result<SingleOutcome> {
    let started = Instant::now();
    let evaluation = client.evaluate(body).await?;
    let mut envelope = envelope(&evaluation, started.elapsed().as_millis() as u64);
    let mut exit_code = 0;
    if let Some(expr) = assert_expr {
        let outcome = expr.evaluate(&envelope)?;
        insert_assert(&mut envelope, &outcome);
        if !outcome.passed {
            exit_code = 3;
        }
    }
    Ok(SingleOutcome {
        exit_code,
        line: serde_json::to_string(&envelope)?,
    })
}

fn envelope(evaluation: &Evaluation, latency_ms: u64) -> Value {
    json!({
        "ok": true,
        "model": evaluation.model,
        "answers": evaluation.answers,
        "usage": {
            "input_tokens": evaluation.usage.input_tokens,
            "output_tokens": evaluation.usage.output_tokens,
        },
        "latency_ms": latency_ms,
    })
}

fn insert_assert(value: &mut Value, outcome: &AssertOutcome) {
    let failed: Vec<Value> = outcome
        .failed
        .iter()
        .map(|f| {
            json!({
                "clause": f.clause,
                "expected": raw_scalar(&f.expected),
                "actual": raw_scalar(&f.actual),
            })
        })
        .collect();
    value["assert"] = json!({"passed": outcome.passed, "failed": failed});
}

/// Failure reports render expected/actual as JSON text; keep numbers as numbers.
fn raw_scalar(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

#[derive(Debug)]
pub struct BatchOutcome {
    pub exit_code: u8,
    pub done: usize,
    pub ok: usize,
    pub failed: usize,
    pub assert_blocked: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

enum Row {
    Done(Evaluation, u64),
    Failed(&'static str, String),
}

fn classify(error: &anyhow::Error) -> (&'static str, String) {
    match error.downcast_ref::<ApiError>() {
        Some(ApiError::Transport(_)) => ("network", format!("{error:#}")),
        Some(_) => ("api", format!("{error:#}")),
        None => ("usage", format!("{error:#}")),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn batch(
    client: Client,
    items: Vec<(usize, Value)>,
    pre_errors: Vec<(usize, String)>,
    assert_expr: Option<&Expr>,
    concurrency: u32,
    out_path: Option<&Path>,
    resume: bool,
) -> Result<BatchOutcome> {
    if resume && out_path.is_none() {
        bail!("--resume requires --out");
    }
    let skip: HashSet<u64> = match (resume, out_path) {
        (true, Some(path)) => read_ok_indices(path)?,
        _ => HashSet::new(),
    };
    let items: Vec<(usize, Value)> = items
        .into_iter()
        .filter(|(index, _)| !skip.contains(&(*index as u64)))
        .collect();
    let mut file = match out_path {
        Some(path) => {
            let handle = if resume && path.exists() {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(path)
                    .with_context(|| format!("failed to open {} for append", path.display()))?
            } else {
                std::fs::File::create(path)
                    .with_context(|| format!("failed to create {}", path.display()))?
            };
            Some(BufWriter::new(handle))
        }
        None => None,
    };

    let semaphore = Arc::new(Semaphore::new(concurrency as usize));
    let mut set: JoinSet<(usize, Row)> = JoinSet::new();
    for (index, body) in items {
        let client = client.clone();
        let semaphore = semaphore.clone();
        set.spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .expect("semaphore is never closed");
            let started = Instant::now();
            let row = match client.evaluate(&body).await {
                Ok(evaluation) => Row::Done(evaluation, started.elapsed().as_millis() as u64),
                Err(error) => {
                    let (kind, message) = classify(&error);
                    Row::Failed(kind, message)
                }
            };
            (index, row)
        });
    }

    let mut outcome = BatchOutcome {
        exit_code: 0,
        done: 0,
        ok: 0,
        failed: 0,
        assert_blocked: 0,
        input_tokens: 0,
        output_tokens: 0,
    };

    for (index, message) in pre_errors {
        outcome.done += 1;
        outcome.failed += 1;
        emit(
            &json!({"index": index, "ok": false, "error": {"kind": "usage", "message": message}}),
            &mut file,
        )?;
    }

    while let Some(joined) = set.join_next().await {
        let (index, row) = joined?;
        outcome.done += 1;
        let line = match row {
            Row::Done(evaluation, latency_ms) => {
                outcome.input_tokens = outcome
                    .input_tokens
                    .saturating_add(evaluation.usage.input_tokens);
                outcome.output_tokens = outcome
                    .output_tokens
                    .saturating_add(evaluation.usage.output_tokens);
                let mut env = envelope(&evaluation, latency_ms);
                env["index"] = json!(index);
                let mut row_ok = true;
                if let Some(expr) = assert_expr {
                    match expr.evaluate(&env) {
                        Ok(assertion) => {
                            insert_assert(&mut env, &assertion);
                            if !assertion.passed {
                                outcome.assert_blocked += 1;
                            }
                        }
                        Err(error) => {
                            row_ok = false;
                            env["ok"] = Value::Bool(false);
                            env["error"] = json!({
                                "kind": "usage",
                                "message": format!("assertion cannot be evaluated: {error:#}"),
                            });
                        }
                    }
                }
                if row_ok {
                    outcome.ok += 1;
                } else {
                    outcome.failed += 1;
                }
                env
            }
            Row::Failed(kind, message) => {
                outcome.failed += 1;
                json!({"index": index, "ok": false, "error": {"kind": kind, "message": message}})
            }
        };
        emit(&line, &mut file)?;
    }

    outcome.exit_code = if outcome.failed > 0 {
        2
    } else if outcome.assert_blocked > 0 {
        3
    } else {
        0
    };
    Ok(outcome)
}

fn emit(line: &Value, file: &mut Option<BufWriter<std::fs::File>>) -> Result<()> {
    let text = serde_json::to_string(line)?;
    {
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(text.as_bytes())
            .and_then(|_| stdout.write_all(b"\n"))
            .and_then(|_| stdout.flush())
            .context("failed to write to stdout")?;
    }
    if let Some(file) = file {
        file.write_all(text.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.flush())
            .with_context(|| "failed to write results file".to_string())?;
    }
    eprint!(".");
    Ok(())
}

fn read_ok_indices(path: &Path) -> Result<HashSet<u64>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut set = HashSet::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)
            .with_context(|| format!("{} line {} is not JSON", path.display(), n + 1))?;
        if value.get("ok").and_then(Value::as_bool) == Some(true)
            && let Some(index) = value.get("index").and_then(Value::as_u64)
        {
            set.insert(index);
        }
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    fn good_response(id: &str, noul: f64) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "model": "jev-latest",
            "answers": {id: {"type": "noul", "noul": noul}},
            "usage": {"input_tokens": 10, "output_tokens": 2}
        }))
    }

    fn request(state: &str) -> Value {
        json!({
            "state": state,
            "model": "jev-latest",
            "questions": {"q": {"type": "noul", "instructions": "yes?"}}
        })
    }

    fn client(server: &MockServer) -> Client {
        api::Client::new("test-key".into(), &server.uri()).unwrap()
    }

    fn temp_out() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.jsonl");
        (dir, out)
    }

    #[tokio::test]
    async fn single_envelope_includes_assert_details() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(good_response("q", 0.93))
            .mount(&server)
            .await;
        let expr = crate::assert::parse("answers.q.noul <= 0.5").unwrap();
        let outcome = single(&client(&server), &request("x"), Some(&expr))
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, 3);
        let line: Value = serde_json::from_str(&outcome.line).unwrap();
        assert_eq!(line["ok"], true);
        assert_eq!(line["assert"]["passed"], false);
        assert_eq!(line["assert"]["failed"][0]["expected"], 0.5);
        assert_eq!(line["assert"]["failed"][0]["actual"], 0.93);
        assert_eq!(line["answers"]["q"]["noul"], 0.93);
        assert_eq!(line["usage"]["input_tokens"], 10);
    }

    #[tokio::test]
    async fn batch_counts_mixed_rows_and_writes_output() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(|req: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                if body["state"] == "bad" {
                    ResponseTemplate::new(422).set_body_string("state invalid")
                } else {
                    good_response("q", 0.9)
                }
            })
            .mount(&server)
            .await;
        let (dir, out) = temp_out();
        let items = vec![(0, request("a")), (1, request("bad")), (2, request("c"))];
        let expr = crate::assert::parse("answers.q.noul >= 0.5").unwrap();
        let outcome = batch(
            client(&server),
            items,
            Vec::new(),
            Some(&expr),
            2,
            Some(&out),
            false,
        )
        .await
        .unwrap();
        assert_eq!(outcome.exit_code, 2);
        assert_eq!(outcome.done, 3);
        assert_eq!(outcome.ok, 2);
        assert_eq!(outcome.failed, 1);
        let lines: Vec<Value> = std::fs::read_to_string(&out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        let failed_row = lines.iter().find(|l| l["ok"] == false).unwrap();
        assert_eq!(failed_row["index"], 1);
        assert_eq!(failed_row["error"]["kind"], "api");
        drop(dir);
    }

    #[tokio::test]
    async fn batch_resume_skips_completed_ok_rows() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(good_response("q", 0.5))
            .expect(2)
            .mount(&server)
            .await;
        let (dir, out) = temp_out();
        std::fs::write(
            &out,
            format!("{}\n", json!({"index": 0, "ok": true, "answers": {}})),
        )
        .unwrap();
        let items = vec![(0, request("a")), (1, request("b")), (2, request("c"))];
        let outcome = batch(
            client(&server),
            items,
            Vec::new(),
            None,
            2,
            Some(&out),
            true,
        )
        .await
        .unwrap();
        assert_eq!(outcome.done, 2);
        assert_eq!(outcome.ok, 2);
        let lines: Vec<Value> = std::fs::read_to_string(&out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3); // pre-existing line + two new
        drop(dir);
    }

    #[tokio::test]
    async fn batch_assert_blocked_yields_exit_three() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(good_response("q", 0.1))
            .mount(&server)
            .await;
        let expr = crate::assert::parse("answers.q.noul >= 0.5").unwrap();
        let outcome = batch(
            client(&server),
            vec![(0, request("a"))],
            Vec::new(),
            Some(&expr),
            1,
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(outcome.exit_code, 3);
        assert_eq!(outcome.assert_blocked, 1);
        assert_eq!(outcome.failed, 0);
    }

    #[tokio::test]
    async fn batch_pre_errors_are_rows_without_requests() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(good_response("q", 0.5))
            .expect(1)
            .mount(&server)
            .await;
        let outcome = batch(
            client(&server),
            vec![(1, request("b"))],
            vec![(0, "line is not a JSON object".to_string())],
            None,
            1,
            None,
            false,
        )
        .await
        .unwrap();
        assert_eq!(outcome.exit_code, 2);
        assert_eq!(outcome.done, 2);
        assert_eq!(outcome.failed, 1);
    }
}
