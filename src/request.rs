//! Request loading: templates from files, stdin, or presets; `{{param}}`
//! substitution; state handling; and the single-question shortcut builders.

use crate::assert;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use std::io::Read as _;

/// Keys a template may carry for documentation and defaults; stripped before send.
const RESERVED_KEYS: [&str; 3] = ["description", "params", "assert"];
const REQUEST_KEYS: [&str; 3] = ["state", "model", "questions"];

#[derive(Debug, Clone)]
pub struct Template {
    /// The request body with reserved keys stripped and params applied.
    pub body: Value,
    pub description: Option<String>,
    /// Parameter names mapped to documentation strings.
    pub params: Map<String, Value>,
    pub default_assert: Option<String>,
}

pub fn parse_template(text: &str, label: &str) -> Result<Template> {
    let value: Value =
        serde_json::from_str(text).with_context(|| format!("invalid JSON in {label}"))?;
    let Value::Object(map) = value else {
        bail!("{label} must be a JSON object describing a request (state, model, questions)");
    };
    let mut map = map;
    let description = take_string(&mut map, "description", label)?;
    let params = match map.remove("params") {
        None => Map::new(),
        Some(Value::Object(p)) => p,
        Some(other) => {
            bail!("{label}: 'params' must be an object of name to description, got {other}")
        }
    };
    let assert_src = take_string(&mut map, "assert", label)?;
    for (key, value) in &params {
        if !value.is_string() {
            bail!("{label}: params.{key} must be a description string");
        }
    }
    let body = Value::Object(map);
    let unknown: Vec<String> = body
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| !REQUEST_KEYS.contains(&k.as_str()))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        bail!(
            "{label}: unknown request key(s) {}; allowed: state, model, questions, plus reserved {}",
            unknown.join(", "),
            RESERVED_KEYS.join("/")
        );
    }
    validate_questions(&body, label)?;
    if let Some(src) = &assert_src {
        assert::parse(src).with_context(|| format!("{label}: invalid 'assert'"))?;
    }
    Ok(Template {
        body,
        description,
        params,
        default_assert: assert_src,
    })
}

fn take_string(map: &mut Map<String, Value>, key: &str, label: &str) -> Result<Option<String>> {
    match map.remove(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => bail!("{label}: reserved key '{key}' must be a string, got {other}"),
    }
}

/// Full pre-send validation: state present, questions well formed.
pub fn validate_ready(body: &Value) -> Result<()> {
    validate_questions(body, "request")?;
    if body.get("state").is_none() {
        bail!("request has no 'state'; add it to the request or pass --state");
    }
    Ok(())
}

pub fn validate_questions(body: &Value, label: &str) -> Result<()> {
    let Some(Value::Object(questions)) = body.get("questions") else {
        bail!("{label}: 'questions' must be a non-empty JSON object of question id to question");
    };
    if questions.is_empty() {
        bail!("{label}: 'questions' must contain at least one question");
    }
    for (id, question) in questions {
        let Some(Value::Object(fields)) = Some(question) else {
            bail!("{label}: questions.{id} must be an object");
        };
        let kind = match fields.get("type") {
            Some(Value::String(kind)) => kind.as_str(),
            _ => bail!("{label}: questions.{id}.type must be \"noul\", \"choice\", or \"score\""),
        };
        let instructions = match fields.get("instructions") {
            Some(instructions) => instructions,
            None => bail!("{label}: questions.{id} is missing 'instructions'"),
        };
        if !matches!(
            instructions,
            Value::String(_) | Value::Object(_) | Value::Array(_)
        ) {
            bail!("{label}: questions.{id}.instructions must be a string, object, or array");
        }
        let criteria = fields.get("criteria");
        match kind {
            "noul" => {
                if let Some(c) = criteria {
                    let Value::Object(entries) = c else {
                        bail!(
                            "{label}: questions.{id}.criteria for a noul must be an object with optional \"true\"/\"false\" strings"
                        );
                    };
                    for (key, value) in entries {
                        if !matches!(key.as_str(), "true" | "false") || !value.is_string() {
                            bail!(
                                "{label}: questions.{id}.criteria keys must be \"true\" or \"false\" mapped to strings"
                            );
                        }
                    }
                }
            }
            "choice" => {
                let Some(Value::Object(entries)) = criteria else {
                    bail!(
                        "{label}: questions.{id} is a choice and requires criteria as an object of option to description (or null)"
                    );
                };
                if entries.is_empty() {
                    bail!("{label}: questions.{id}.criteria needs at least one option");
                }
                for (option, value) in entries {
                    if !value.is_string() && !value.is_null() {
                        bail!(
                            "{label}: questions.{id}.criteria.{option} must be a description string or null"
                        );
                    }
                }
            }
            "score" => {
                let Some(Value::Array(levels)) = criteria else {
                    bail!(
                        "{label}: questions.{id} is a score and requires criteria as an ordered array of level descriptions"
                    );
                };
                if levels.len() < 2 {
                    bail!("{label}: questions.{id}.criteria needs at least two ordered levels");
                }
                for (i, level) in levels.iter().enumerate() {
                    if !level.is_string() {
                        bail!(
                            "{label}: questions.{id}.criteria[{i}] must be a level description string"
                        );
                    }
                }
            }
            other => bail!(
                "{label}: questions.{id}.type is \"{other}\"; it must be \"noul\", \"choice\", or \"score\""
            ),
        }
    }
    if let Some(model) = body.get("model")
        && !model.is_string()
    {
        bail!("{label}: 'model' must be a string");
    }
    Ok(())
}

/// Replace `{{name}}` placeholders. A placeholder must be the entire string
/// value of a field; the parameter value (string or parsed JSON) replaces it.
pub fn apply_params(body: &mut Value, params: &[(String, Value)]) -> Result<()> {
    let mut occurrences: Vec<(String, String)> = Vec::new(); // (placeholder, path)
    collect_placeholders(body, String::new(), &mut occurrences);
    let mut used = vec![false; params.len()];
    let mut missing = Vec::new();
    for (name, path) in &occurrences {
        match params.iter().position(|(k, _)| k == name) {
            Some(i) => {
                used[i] = true;
                set_path(body, path, params[i].1.clone());
            }
            None => missing.push(format!("{name} (needed at {path})")),
        }
    }
    if !missing.is_empty() {
        bail!(
            "missing --param value(s): {}; pass them as --param NAME=VALUE or --param NAME=@PATH",
            missing.join(", ")
        );
    }
    for (used, (name, _)) in used.iter().zip(params) {
        if !used {
            crate::diagnostics::log(format_args!(
                "Warning: --param {name} is not referenced by any placeholder in the template"
            ));
        }
    }
    Ok(())
}

fn collect_placeholders(value: &Value, path: String, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                collect_placeholders(v, join_path(&path, k), out);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                collect_placeholders(v, format!("{path}[{i}]"), out);
            }
        }
        Value::String(s) => {
            if let Some(name) = placeholder_name(s) {
                out.push((name.to_string(), path));
            }
        }
        _ => {}
    }
}

fn placeholder_name(s: &str) -> Option<&str> {
    s.strip_prefix("{{")
        .and_then(|rest| rest.strip_suffix("}}"))
}

fn set_path(root: &mut Value, path: &str, value: Value) {
    // Placeholders are always entire string leaves, so splitting on '.' and
    // descending objects (and "[i]" array segments) reaches the exact leaf.
    assign_leaf(root, path, value);
}

fn assign_leaf(root: &mut Value, path: &str, value: Value) {
    let mut segments: Vec<&str> = path.split('.').collect();
    let last = segments.pop().expect("placeholder path is never empty");
    let mut current = root;
    for seg in segments {
        current = descend(current, seg);
    }
    let (key, index) = split_index(last);
    let map = current.as_object_mut().unwrap();
    let child = map.get_mut(key).unwrap();
    match index {
        Some(i) => {
            child.as_array_mut().unwrap()[i] = value;
        }
        None => {
            *child = value;
        }
    }
}

fn descend<'a>(value: &'a mut Value, seg: &str) -> &'a mut Value {
    let (key, index) = split_index(seg);
    let map = value.as_object_mut().unwrap();
    let child = map.get_mut(key).unwrap();
    match index {
        Some(i) => child.as_array_mut().unwrap().get_mut(i).unwrap(),
        None => child,
    }
}

fn split_index(seg: &str) -> (&str, Option<usize>) {
    match seg.find('[') {
        Some(i) => (&seg[..i], Some(seg[i + 1..seg.len() - 1].parse().unwrap())),
        None => (seg, None),
    }
}

fn join_path(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

/// Parse `NAME=VALUE`, `NAME=@PATH`, or `NAME=@-` (stdin). Files that contain
/// valid JSON become structured values; anything else becomes a string.
pub fn parse_param(spec: &str) -> Result<(String, Value)> {
    let (name, value) = spec.split_once('=').ok_or_else(|| {
        anyhow::anyhow!("invalid --param '{spec}'; expected NAME=VALUE or NAME=@PATH")
    })?;
    if name.is_empty() {
        bail!("invalid --param '{spec}'; the name before '=' must not be empty");
    }
    let value = if let Some(path) = value.strip_prefix('@') {
        read_interpreted(path)?
    } else {
        Value::String(value.to_string())
    };
    Ok((name.to_string(), value))
}

fn read_interpreted(path: &str) -> Result<Value> {
    let bytes = if path == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .context("failed to read stdin for a @- parameter")?;
        buf
    } else {
        std::fs::read(path).with_context(|| format!("failed to read param file {path}"))?
    };
    interpret_bytes(&bytes, &format!("@{path}"))
}

/// Objects and arrays stay structured; other text becomes a JSON string.
pub fn interpret_bytes(bytes: &[u8], label: &str) -> Result<Value> {
    let first = bytes.iter().find(|b| !b.is_ascii_whitespace()).copied();
    match first {
        Some(b'{') | Some(b'[') => {
            serde_json::from_slice(bytes).with_context(|| format!("invalid JSON in {label}"))
        }
        _ => Ok(Value::String(
            String::from_utf8_lossy(bytes)
                .trim_end_matches(['\r', '\n'])
                .to_string(),
        )),
    }
}

/// Load state for shortcut/ask `--state` and stdin input.
pub fn load_state_value(arg: Option<&str>) -> Result<Value> {
    match arg {
        Some(spec) => {
            if let Some(path) = spec.strip_prefix('@') {
                read_interpreted(path)
            } else {
                Ok(Value::String(spec.to_string()))
            }
        }
        None => {
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                bail!("state is required: pass --state TEXT, --state @PATH, or pipe it on stdin");
            }
            let mut buf = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buf)
                .context("failed to read state from stdin")?;
            interpret_bytes(&buf, "stdin")
        }
    }
}

pub fn read_stdin(label: &str) -> Result<String> {
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!("expected input on stdin for {label}; pipe a file or pass a PATH instead");
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .with_context(|| format!("failed to read {label} from stdin"))?;
    Ok(buf)
}

pub fn shortcut_noul(question: &str, yes: Option<&str>, no: Option<&str>, state: Value) -> Value {
    let mut q = serde_json::json!({"type": "noul", "instructions": question});
    if yes.is_some() || no.is_some() {
        let true_desc = yes.unwrap_or("A yes (value near 1)");
        let false_desc = no.unwrap_or("A no (value near 0)");
        q["criteria"] = serde_json::json!({"true": true_desc, "false": false_desc});
    }
    finish_shortcut(q, state)
}

pub fn shortcut_choice(question: &str, opts: &[(String, String)], state: Value) -> Value {
    let criteria: Map<String, Value> = opts
        .iter()
        .map(|(k, d)| (k.clone(), Value::String(d.clone())))
        .collect();
    let q = serde_json::json!({
        "type": "choice",
        "instructions": question,
        "criteria": Value::Object(criteria),
    });
    finish_shortcut(q, state)
}

pub fn shortcut_score(question: &str, levels: &[String], state: Value) -> Value {
    let q = serde_json::json!({
        "type": "score",
        "instructions": question,
        "criteria": levels,
    });
    finish_shortcut(q, state)
}

fn finish_shortcut(question: Value, state: Value) -> Value {
    serde_json::json!({
        "state": state,
        "model": crate::api::DEFAULT_MODEL,
        "questions": {"q": question},
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn template() -> Value {
        json!({
            "state": {"issue": "{{issue}}", "repo": "{{repo}}"},
            "questions": {
                "invalid": {"type": "noul", "instructions": "Is this report invalid?", "criteria": {"true": "spam or duplicate", "false": "a real report"}}
            },
            "assert": "answers.invalid.noul <= 0.5"
        })
    }

    #[test]
    fn parse_template_strips_reserved_and_validates() {
        let t = parse_template(&template().to_string(), "t.json").unwrap();
        assert_eq!(
            t.default_assert.as_deref(),
            Some("answers.invalid.noul <= 0.5")
        );
        assert!(t.body.get("assert").is_none());
        assert!(t.body.get("questions").is_some());
    }

    #[test]
    fn rejects_unknown_request_keys() {
        let mut v = template();
        v["templatename"] = json!("x");
        let err = parse_template(&v.to_string(), "t.json").unwrap_err();
        assert!(err.to_string().contains("unknown request key"), "{err}");
    }

    #[test]
    fn rejects_bad_question_shapes() {
        for broken in [
            json!({"questions": {}}),
            json!({"questions": {"a": {"instructions": "x"}}}),
            json!({"questions": {"a": {"type": "vector", "instructions": "x"}}}),
            json!({"questions": {"a": {"type": "score", "instructions": "x", "criteria": ["only"]}}}),
            json!({"questions": {"a": {"type": "choice", "instructions": "x"}}}),
        ] {
            assert!(validate_questions(&broken, "req").is_err(), "{broken}");
        }
    }

    #[test]
    fn params_inject_subtrees_and_report_missing() {
        let t = parse_template(&template().to_string(), "t.json").unwrap();
        let mut body = t.body;
        apply_params(
            &mut body,
            &[
                (
                    "issue".into(),
                    json!({"title": "crash", "body": "it crashes"}),
                ),
                ("repo".into(), Value::String("acme/api".into())),
            ],
        )
        .unwrap();
        assert_eq!(body["state"]["issue"]["title"], "crash");
        assert_eq!(body["state"]["repo"], "acme/api");

        let t = parse_template(&template().to_string(), "t.json").unwrap();
        let mut body = t.body;
        let err = apply_params(&mut body, &[]).unwrap_err();
        assert!(err.to_string().contains("missing --param"), "{err}");
        assert!(err.to_string().contains("needed at state.issue"));
    }

    #[test]
    fn placeholder_inside_array_is_replaced() {
        let mut body = json!({"state": {"tags": ["{{a}}", "fixed"]}, "questions": {}});
        validate_questions(&body.clone(), "req").ok();
        apply_params(&mut body, &[("a".into(), Value::String("x".into()))]).unwrap();
        assert_eq!(body["state"]["tags"][0], "x");
    }

    #[test]
    fn interpret_bytes_keeps_objects_and_wraps_text() {
        assert_eq!(
            interpret_bytes(b"  {\"a\": 1}", "x").unwrap(),
            json!({"a": 1})
        );
        assert_eq!(interpret_bytes(b"plain text\n", "x").unwrap(), "plain text");
        assert!(interpret_bytes(b"{bad", "x").is_err());
    }

    #[test]
    fn shortcuts_build_valid_requests() {
        let body = shortcut_noul("urgent?", Some("yes means now"), None, json!("help"));
        assert_eq!(body["questions"]["q"]["criteria"]["true"], "yes means now");
        assert_eq!(body["state"], "help");
        assert_eq!(body["model"], "jev-latest");
        validate_ready(&body).unwrap();
        let body = shortcut_choice("route?", &[("billing".into(), "money".into())], json!("x"));
        validate_ready(&body).unwrap();
        let body = shortcut_score(
            "how bad?",
            &["fine".into(), "bad".into()],
            Value::String("x".into()),
        );
        validate_ready(&body).unwrap();
    }

    #[test]
    fn parse_param_forms() {
        assert_eq!(
            parse_param("k=v").unwrap(),
            ("k".into(), Value::String("v".into()))
        );
        assert!(parse_param("novalue").is_err());
        assert!(parse_param("=v").is_err());
    }
}
