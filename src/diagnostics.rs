use std::{
    fmt,
    io::{self, Write as _},
    mem,
    sync::{Mutex, OnceLock},
};
use tempfile::{Builder, NamedTempFile};

enum Sink {
    Stderr,
    Captured(NamedTempFile),
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

pub fn init(capture: bool) {
    let sink = if capture {
        match Builder::new()
            .prefix("jev-diagnostics-")
            .suffix(".log")
            .tempfile()
        {
            Ok(file) => Sink::Captured(file),
            Err(error) => {
                write_stderr(format_args!(
                    "Warning: failed to create diagnostics file: {error}; using stderr"
                ));
                Sink::Stderr
            }
        }
    } else {
        Sink::Stderr
    };
    let _ = SINK.set(Mutex::new(sink));
}

pub fn log(args: fmt::Arguments<'_>) {
    let Some(sink) = SINK.get() else {
        write_stderr(args);
        return;
    };
    let mut sink = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match &mut *sink {
        Sink::Stderr => write_stderr(args),
        Sink::Captured(file) => {
            let _ = writeln!(file.as_file_mut(), "{args}");
        }
    }
}

pub fn finish(exit_code: u8) {
    let Some(sink) = SINK.get() else {
        return;
    };
    let captured = {
        let mut sink = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match mem::replace(&mut *sink, Sink::Stderr) {
            Sink::Stderr => return,
            Sink::Captured(mut file) => {
                let _ = file.as_file_mut().flush();
                file
            }
        }
    };
    if !retain(exit_code) {
        return;
    }
    match captured.keep() {
        Ok((file, path)) => {
            drop(file);
            write_stderr(format_args!("JEV_DIAGNOSTICS={}", path.display()));
        }
        Err(error) => write_stderr(format_args!(
            "Warning: failed to retain diagnostics file: {error}"
        )),
    }
}

fn retain(exit_code: u8) -> bool {
    // 0, 2, and 3 are valid automation outcomes, not defects worth a diagnostics dump.
    !matches!(exit_code, 0 | 2 | 3)
}

fn write_stderr(args: fmt::Arguments<'_>) {
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "{args}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_only_error_and_interruption_diagnostics() {
        assert!(!retain(0));
        assert!(!retain(2));
        assert!(!retain(3));
        assert!(retain(1));
        assert!(retain(130));
    }
}
