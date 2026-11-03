use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, Result};

use crate::config::ShellItem;

#[derive(Clone, Debug)]
pub struct ShellExecutor {
    shell: OsString,
    source_cwd: PathBuf,
}

impl ShellExecutor {
    pub fn new(shell: impl Into<OsString>, source_cwd: impl Into<PathBuf>) -> Self {
        Self {
            shell: shell.into(),
            source_cwd: source_cwd.into(),
        }
    }

    pub fn from_environment(source_cwd: impl Into<PathBuf>) -> Self {
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
        Self::new(shell, source_cwd)
    }

    pub fn working_directory(&self, item: &ShellItem) -> PathBuf {
        match &item.cwd {
            Some(cwd) if cwd.is_absolute() => cwd.clone(),
            Some(cwd) => self.source_cwd.join(cwd),
            None => self.source_cwd.clone(),
        }
    }

    pub fn foreground(&self, item: &ShellItem) -> Result<ExitStatus> {
        Command::new(&self.shell)
            .arg("-lc")
            .arg(&item.command)
            .current_dir(self.working_directory(item))
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("failed to start shell")
    }

    pub fn detached(&self, item: &ShellItem) -> Result<()> {
        let mut command = Command::new("/usr/bin/nohup");
        command
            .arg(&self.shell)
            .arg("-lc")
            .arg(&item.command)
            .current_dir(self.working_directory(item))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        command.spawn().context("failed to start detached shell")?;
        Ok(())
    }

    pub fn source_cwd(&self) -> &Path {
        &self.source_cwd
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::thread;
    use std::time::{Duration, Instant};

    use tempfile::tempdir;

    use super::*;
    use crate::config::ExecutionMode;

    fn item(command: impl Into<String>) -> ShellItem {
        ShellItem {
            key: 'x',
            label: "test".into(),
            command: command.into(),
            mode: ExecutionMode::Foreground,
            confirm: None,
            cwd: None,
        }
    }

    #[test]
    fn foreground_inherits_environment_and_source_directory() {
        let dir = tempdir().unwrap();
        let executor = ShellExecutor::new("/bin/sh", dir.path());
        let status = executor
            .foreground(&item("printf '%s\\n%s' \"$PWD\" \"$USER\" > result"))
            .unwrap();
        let result = fs::read_to_string(dir.path().join("result")).unwrap();
        let lines = result.lines().collect::<Vec<_>>();

        assert!(status.success());
        assert_eq!(
            fs::canonicalize(lines[0]).unwrap(),
            fs::canonicalize(dir.path()).unwrap()
        );
        assert_eq!(lines[1], std::env::var("USER").unwrap());
    }

    #[test]
    fn relative_and_absolute_item_directories_override_the_source() {
        let dir = tempdir().unwrap();
        let relative = dir.path().join("relative");
        let absolute = dir.path().join("absolute");
        fs::create_dir_all(&relative).unwrap();
        fs::create_dir_all(&absolute).unwrap();
        let executor = ShellExecutor::new("/bin/sh", dir.path());
        let mut relative_item = item("true");
        relative_item.cwd = Some(PathBuf::from("relative"));
        let mut absolute_item = item("true");
        absolute_item.cwd = Some(absolute.clone());

        assert_eq!(executor.working_directory(&relative_item), relative);
        assert_eq!(executor.working_directory(&absolute_item), absolute);
    }

    #[test]
    fn foreground_preserves_nonzero_exit_status() {
        let dir = tempdir().unwrap();
        let executor = ShellExecutor::new("/bin/sh", dir.path());

        let status = executor.foreground(&item("exit 7")).unwrap();

        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn missing_shell_is_a_spawn_error() {
        let dir = tempdir().unwrap();
        let executor = ShellExecutor::new(dir.path().join("missing-shell"), dir.path());

        let error = executor.foreground(&item("true")).unwrap_err().to_string();

        assert!(error.contains("failed to start shell"));
    }

    #[test]
    fn detached_command_survives_the_executor_returning() {
        let dir = tempdir().unwrap();
        let marker = dir.path().join("detached-result");
        let executor = ShellExecutor::new("/bin/sh", dir.path());
        let command = format!("sleep 0.1; printf done > '{}'", marker.display());
        let started = Instant::now();

        executor.detached(&item(command)).unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
        let deadline = Instant::now() + Duration::from_secs(3);
        while !marker.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(fs::read_to_string(marker).unwrap(), "done");
    }
}
