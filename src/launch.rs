//! Builds the per-instance DSH launch contract without starting a process.

use crate::instances::{AgentsHomeMode, InstanceConfig, InstancePaths, RepositoryError};
use chrono::Utc;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub executable: PathBuf,
    pub working_directory: PathBuf,
    pub arguments: Vec<OsString>,
    pub environment: Vec<(OsString, OsString)>,
    pub environment_to_remove: Vec<OsString>,
    pub diagnostic_file: PathBuf,
}

impl LaunchSpec {
    pub fn for_instance(
        executable: impl Into<PathBuf>,
        config: &InstanceConfig,
        paths: &InstancePaths,
    ) -> Result<Self, LaunchSpecError> {
        config.validate()?;
        if paths.root.file_name() != Some(OsStr::new(&config.id)) {
            return Err(LaunchSpecError::InstancePathMismatch {
                id: config.id.clone(),
                root: paths.root.clone(),
            });
        }

        let executable = executable.into();
        let working_directory = executable
            .parent()
            .ok_or_else(|| LaunchSpecError::ExecutableHasNoParent(executable.clone()))?
            .to_path_buf();
        let diagnostic_file = paths
            .marina
            .join(format!("diagnostic-{}.txt", Utc::now().timestamp_millis()));

        let mut user_data_argument = OsString::from("--user-data-dir=");
        user_data_argument.push(paths.electron.as_os_str());

        let mut environment = vec![
            (
                OsString::from("DSH_HOME"),
                paths.home.as_os_str().to_os_string(),
            ),
            (
                OsString::from("DSH_DESKTOP_DIAGNOSTIC_FILE"),
                diagnostic_file.as_os_str().to_os_string(),
            ),
        ];
        if config.isolation.agents_home == AgentsHomeMode::Isolated {
            environment.push((
                OsString::from("DSH_AGENTS_HOME"),
                paths.agents.as_os_str().to_os_string(),
            ));
        }

        Ok(Self {
            executable,
            working_directory,
            arguments: vec![user_data_argument],
            environment,
            environment_to_remove: vec![OsString::from("ELECTRON_RUN_AS_NODE")],
            diagnostic_file,
        })
    }
}

#[derive(Debug, Error)]
pub enum LaunchSpecError {
    #[error(transparent)]
    InvalidInstance(#[from] RepositoryError),
    #[error("实例 ID {id} 和它所在的目录 {root} 对不上")]
    InstancePathMismatch { id: String, root: PathBuf },
    #[error("DSH 可执行文件没有上级目录：{0}")]
    ExecutableHasNoParent(PathBuf),
}

/// Redacts the value of each `token=` query parameter before captured output is logged.
pub fn redact_tokens(line: &str) -> String {
    let mut redacted = line.to_owned();
    let mut search_from = 0;

    while let Some(relative_start) = redacted[search_from..].find("token=") {
        let value_start = search_from + relative_start + "token=".len();
        let value_end = redacted[value_start..]
            .find(['&', '#', ' ', '\t', '\r', '\n'])
            .map(|offset| value_start + offset)
            .unwrap_or(redacted.len());
        if value_end == value_start {
            search_from = value_start;
            continue;
        }
        redacted.replace_range(value_start..value_end, "***");
        search_from = value_start + 3;
    }

    redacted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instances::InstanceRepository;
    use tempfile::tempdir;

    #[test]
    fn launch_spec_uses_isolated_paths_and_removes_electron_run_as_node() {
        let temp = tempdir().unwrap();
        let repository = InstanceRepository::new(temp.path().join("data"));
        let config = repository.create_blank("工作区").unwrap();
        let paths = repository.paths(&config.id).unwrap();
        let executable = temp
            .path()
            .join("DeepSeek Harness")
            .join("DeepSeek Harness.exe");

        let spec = LaunchSpec::for_instance(&executable, &config, &paths).unwrap();
        assert_eq!(spec.executable, executable);
        assert_eq!(spec.working_directory, temp.path().join("DeepSeek Harness"));
        assert_eq!(spec.arguments.len(), 1);
        assert_eq!(
            spec.arguments[0],
            OsString::from(format!("--user-data-dir={}", paths.electron.display()))
        );
        assert!(spec.environment.contains(&(
            OsString::from("DSH_HOME"),
            paths.home.as_os_str().to_os_string()
        )));
        assert!(spec.environment.contains(&(
            OsString::from("DSH_DESKTOP_DIAGNOSTIC_FILE"),
            spec.diagnostic_file.as_os_str().to_os_string()
        )));
        assert!(
            spec.environment_to_remove
                .contains(&OsString::from("ELECTRON_RUN_AS_NODE"))
        );
        assert!(
            !spec
                .environment
                .iter()
                .any(|(key, _)| key == OsStr::new("DSH_AGENTS_HOME"))
        );
    }

    #[test]
    fn isolated_agents_mode_adds_instance_agents_directory() {
        let temp = tempdir().unwrap();
        let repository = InstanceRepository::new(temp.path().join("data"));
        let mut config = repository.create_blank("工作区").unwrap();
        config.isolation.agents_home = AgentsHomeMode::Isolated;
        let paths = repository.paths(&config.id).unwrap();

        let spec =
            LaunchSpec::for_instance("C:\\DSH\\DeepSeek Harness.exe", &config, &paths).unwrap();
        assert!(spec.environment.contains(&(
            OsString::from("DSH_AGENTS_HOME"),
            paths.agents.as_os_str().to_os_string()
        )));
    }

    #[test]
    fn rejects_instance_paths_that_do_not_match_the_config_id() {
        let temp = tempdir().unwrap();
        let repository = InstanceRepository::new(temp.path().join("data"));
        let config = repository.create_blank("工作区").unwrap();
        let mut paths = repository.paths(&config.id).unwrap();
        paths.root = temp.path().join("some-other-instance");

        assert!(matches!(
            LaunchSpec::for_instance("C:\\DSH\\DeepSeek Harness.exe", &config, &paths),
            Err(LaunchSpecError::InstancePathMismatch { .. })
        ));
    }

    #[test]
    fn redacts_token_values_without_losing_other_query_parameters() {
        assert_eq!(
            redact_tokens("dsh web: http://127.0.0.1:1234/?token=secret&mode=desktop"),
            "dsh web: http://127.0.0.1:1234/?token=***&mode=desktop"
        );
    }
}
