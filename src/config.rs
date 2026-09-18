use anyhow::{Context, Result, bail};
use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::{fs, path::PathBuf};

#[derive(Serialize, Deserialize)]
struct Credentials {
    version: u32,
    key: String,
}

fn path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("Unable to determine home directory")?
        .join("model-clis/jev/credentials.json"))
}

fn load_key_at(env_key: Option<String>, target: &std::path::Path) -> Result<String> {
    if let Some(key) = env_key.filter(|k| !k.trim().is_empty()) {
        return Ok(key);
    }
    let c: Credentials =
        serde_json::from_slice(&fs::read(target).context("Not logged in; run jev login")?)
            .context("Invalid credentials file")?;
    if c.version != 1 || c.key.trim().is_empty() {
        bail!("Invalid credentials file")
    }
    Ok(c.key)
}

pub fn load_key() -> Result<String> {
    load_key_at(std::env::var("JEV_API_KEY").ok(), &path()?)
}

pub fn save_key(key: &str) -> Result<()> {
    let target = path()?;
    let parent = target.parent().unwrap();
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    let mut file = AtomicWriteFile::options();
    #[cfg(not(unix))]
    let file = AtomicWriteFile::options();
    #[cfg(unix)]
    {
        use atomic_write_file::unix::OpenOptionsExt as _;
        file.preserve_mode(false);
        use std::os::unix::fs::OpenOptionsExt as _;
        file.mode(0o600);
    }
    let mut file = file.open(&target)?;
    file.write_all(&serde_json::to_vec(&Credentials {
        version: 1,
        key: key.into(),
    })?)?;
    file.commit()?;
    Ok(())
}

pub fn logout() -> Result<()> {
    let p = path()?;
    match fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_key_wins_and_blank_env_falls_back_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("credentials.json");
        save_key_at_for_test(&target, "file-key").unwrap();
        assert_eq!(
            load_key_at(Some("env-key".into()), &target).unwrap(),
            "env-key"
        );
        assert_eq!(load_key_at(Some("  ".into()), &target).unwrap(), "file-key");
        assert_eq!(load_key_at(None, &target).unwrap(), "file-key");
    }

    #[test]
    fn missing_file_is_a_login_hint() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_key_at(None, &dir.path().join("none.json")).unwrap_err();
        assert!(err.to_string().contains("run jev login"));
    }

    fn save_key_at_for_test(target: &std::path::Path, key: &str) -> Result<()> {
        let mut file = AtomicWriteFile::options().open(target)?;
        file.write_all(&serde_json::to_vec(&Credentials {
            version: 1,
            key: key.into(),
        })?)?;
        file.commit()?;
        Ok(())
    }
}
