//! Preset discovery: named request templates in `./.jev/presets` (repository)
//! and the user config directory. Repository presets win on name collision.

use crate::request::{self, Template};
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

pub fn dirs() -> Result<Vec<PathBuf>> {
    let mut found = vec![
        std::env::current_dir()
            .context("unable to determine the current directory")?
            .join(".jev/presets"),
    ];
    if let Some(config) = dirs::config_dir() {
        found.push(config.join("jev/presets"));
    }
    Ok(found)
}

pub struct Found {
    pub name: String,
    pub path: PathBuf,
    pub description: Option<String>,
}

pub fn list() -> Result<Vec<Found>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for dir in dirs()? {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_file() && matches!(p.extension().and_then(|e| e.to_str()), Some("json"))
            })
            .collect();
        files.sort();
        for path in files {
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !seen.insert(name.to_string()) {
                continue;
            }
            let description = read_description(&path);
            out.push(Found {
                name: name.to_string(),
                path,
                description,
            });
        }
    }
    Ok(out)
}

fn read_description(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    request::parse_template(&text, &path.display().to_string())
        .ok()
        .and_then(|t| t.description)
}

pub fn resolve(name: &str) -> Result<PathBuf> {
    for dir in dirs()? {
        let candidate = dir.join(format!("{name}.json"));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    let searched = dirs()?
        .iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    bail!("preset '{name}' not found; searched: {searched} (list with jev presets list)");
}

pub fn load(path: &std::path::Path) -> Result<Template> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read preset {}", path.display()))?;
    request::parse_template(&text, &format!("preset {}", path.display()))
}

pub fn load_by_name(name: &str) -> Result<Template> {
    load(&resolve(name)?)
}

pub fn show(name: &str) -> Result<String> {
    let path = resolve(name)?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read preset {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("preset {} is not valid JSON", path.display()))?;
    Ok(serde_json::to_string_pretty(&value)?)
}

/// Validate one target (a preset name or a file path) or every discovered
/// preset when no target is given. Returns the number of invalid presets.
pub fn validate(target: Option<&str>) -> Result<usize> {
    let targets: Vec<(String, PathBuf)> = match target {
        Some(name) => {
            let path = std::path::Path::new(name);
            if path.is_file() {
                vec![(name.to_string(), path.to_path_buf())]
            } else {
                vec![(name.to_string(), resolve(name)?)]
            }
        }
        None => list()?.into_iter().map(|f| (f.name, f.path)).collect(),
    };
    if targets.is_empty() {
        bail!("no presets found; create .jev/presets/NAME.json or ~/.config/jev/presets/NAME.json");
    }
    let mut invalid = 0;
    for (name, path) in targets {
        match load(&path) {
            Ok(template) => {
                println!("ok {name} ({})", path.display());
                if template.params.is_empty() {
                    let placeholders = count_placeholders(&template.body);
                    if placeholders > 0 {
                        println!(
                            "  note: {placeholders} placeholder(s) but no 'params' documentation"
                        );
                    }
                }
            }
            Err(e) => {
                invalid += 1;
                println!("error {name} ({}): {e:#}", path.display());
            }
        }
    }
    Ok(invalid)
}

fn count_placeholders(body: &serde_json::Value) -> usize {
    fn walk(v: &serde_json::Value, count: &mut usize) {
        match v {
            serde_json::Value::Object(map) => {
                for (_, child) in map {
                    walk(child, count);
                }
            }
            serde_json::Value::Array(items) => {
                for child in items {
                    walk(child, count);
                }
            }
            serde_json::Value::String(s) => {
                if s.starts_with("{{") && s.ends_with("}}") && s.len() > 4 {
                    *count += 1;
                }
            }
            _ => {}
        }
    }
    let mut count = 0;
    walk(body, &mut count);
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_reports_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("broken.json");
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(validate(Some(p.to_str().unwrap())).unwrap(), 1);
    }

    #[test]
    fn validate_accepts_a_good_template() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("good.json");
        std::fs::write(
            &p,
            r#"{"description":"d","questions":{"q":{"type":"noul","instructions":"x"}}}"#,
        )
        .unwrap();
        assert_eq!(validate(Some(p.to_str().unwrap())).unwrap(), 0);
    }

    #[test]
    fn resolve_names_the_searched_dirs() {
        let err = resolve("definitely-not-there-xyz").unwrap_err();
        assert!(err.to_string().contains("not found"));
        assert!(err.to_string().contains(".jev/presets"));
    }
}
