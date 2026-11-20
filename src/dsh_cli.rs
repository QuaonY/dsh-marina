//! Runs DSH's bundled CLI (`dsh plugin --profile desktop <pnpm-args>`)
//! against one instance's `DSH_HOME` (design §FR-3, §FR-4, §7.4).
//!
//! The desktop install ships `resources\runtime\cli\bin\dsh.cmd`, which runs
//! `DeepSeek Harness.exe` as Node (`ELECTRON_RUN_AS_NODE=1`) with
//! `resources\app.asar\dsh\node_modules\@deepseek-ai\dsh-desktop-host\lib\cli.js`.
//! Marina invokes the same entry directly so no console window or `cmd.exe`
//! quoting is involved.  The instance must be stopped: DSH holds a profile
//! lock while running.

use crate::launch::redact_tokens;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use thiserror::Error;

const CLI_ENTRY: [&str; 7] = [
    "app.asar",
    "dsh",
    "node_modules",
    "@deepseek-ai",
    "dsh-desktop-host",
    "lib",
    "cli.js",
];

/// Default limit for one CLI call.  `install` may download packages.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const OUTPUT_TAIL_LINES: usize = 40;

#[derive(Debug, Error)]
pub enum DshCliError {
    #[error("这个 DSH 运行时没有自带命令行工具（{0}）")]
    Unavailable(PathBuf),
    #[error("启动 DSH 命令行工具失败：{0}")]
    Spawn(#[source] std::io::Error),
    #[error("DSH 命令行工具在 {0} 秒内没有结束，已停止")]
    TimedOut(u64),
    #[error("dsh plugin {command} 执行失败（退出码 {code:?}）：{output}")]
    Failed {
        command: String,
        code: Option<i32>,
        output: String,
    },
    #[error("插件包名不合法：{0}")]
    InvalidPackage(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliOutcome {
    /// Last lines of combined stdout/stderr, tokens redacted.
    pub output: String,
}

/// Command line pieces for `dsh plugin --profile desktop <args>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommand {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub dsh_home: PathBuf,
}

impl PluginCommand {
    pub fn new(dsh_executable: &Path, dsh_home: &Path, args: &[&str]) -> Result<Self, DshCliError> {
        let entry = cli_entry(dsh_executable)?;
        let mut arguments = vec![OsString::from("--expose-internals"), entry.into_os_string()];
        arguments.extend(["plugin", "--profile", "desktop"].map(OsString::from));
        arguments.extend(args.iter().map(OsString::from));
        Ok(Self {
            executable: dsh_executable.to_path_buf(),
            arguments,
            dsh_home: dsh_home.to_path_buf(),
        })
    }
}

/// Path of the CLI entry inside the runtime's `app.asar`.
pub fn cli_entry(dsh_executable: &Path) -> Result<PathBuf, DshCliError> {
    let resources = crate::runtimes::resources_dir_for_executable(dsh_executable)
        .ok_or_else(|| DshCliError::Unavailable(dsh_executable.to_path_buf()))?;
    let archive = resources.join("app.asar");
    if !archive.exists() {
        return Err(DshCliError::Unavailable(archive));
    }
    Ok(CLI_ENTRY
        .iter()
        .fold(resources, |path, component| path.join(component)))
}

pub fn is_available(dsh_executable: &Path) -> bool {
    cli_entry(dsh_executable).is_ok()
}

/// `dsh plugin --profile desktop install`: make node_modules match the
/// restored `package.json` / lock file.
pub fn install(dsh_executable: &Path, dsh_home: &Path) -> Result<CliOutcome, DshCliError> {
    run(
        &PluginCommand::new(dsh_executable, dsh_home, &["install"])?,
        DEFAULT_TIMEOUT,
    )
}

/// `dsh plugin --profile desktop remove <package>`.
pub fn remove(
    dsh_executable: &Path,
    dsh_home: &Path,
    package: &str,
) -> Result<CliOutcome, DshCliError> {
    validate_package_name(package)?;
    run(
        &PluginCommand::new(dsh_executable, dsh_home, &["remove", package])?,
        DEFAULT_TIMEOUT,
    )
}

/// `dsh plugin --profile desktop add <spec>...` (design §FR-3 offline
/// install).  Each spec is either a local `.tgz` file (made absolute) or a
/// registry spec `name` / `name@version-or-tag`.
pub fn add_packages(
    dsh_executable: &Path,
    dsh_home: &Path,
    specs: &[&str],
) -> Result<CliOutcome, DshCliError> {
    let specs = add_arguments(specs)?;
    let mut args = vec!["add"];
    args.extend(specs.iter().map(String::as_str));
    run(
        &PluginCommand::new(dsh_executable, dsh_home, &args)?,
        DEFAULT_TIMEOUT,
    )
}

/// Validate and normalize `add` specs; see [`add_packages`].
pub fn add_arguments(specs: &[&str]) -> Result<Vec<String>, DshCliError> {
    if specs.is_empty() {
        return Err(DshCliError::InvalidPackage(String::new()));
    }
    specs
        .iter()
        .map(|spec| {
            let spec = spec.trim();
            if spec.starts_with('-') {
                return Err(DshCliError::InvalidPackage(spec.to_owned()));
            }
            let path = Path::new(spec);
            let is_tarball = path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("tgz"));
            if is_tarball {
                let absolute = std::path::absolute(path)
                    .map_err(|_| DshCliError::InvalidPackage(spec.to_owned()))?;
                if !absolute.is_file() {
                    return Err(DshCliError::InvalidPackage(spec.to_owned()));
                }
                return Ok(absolute.to_string_lossy().into_owned());
            }
            let (name, version) = split_version(spec);
            validate_package_name(name)?;
            if let Some(version) = version
                && (version.is_empty()
                    || version.len() > 128
                    || !version
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || ".-+_^~<>=*|".contains(c)))
            {
                return Err(DshCliError::InvalidPackage(spec.to_owned()));
            }
            Ok(spec.to_owned())
        })
        .collect()
}

/// `@scope/name@1.0` → (`@scope/name`, Some(`1.0`)).
fn split_version(spec: &str) -> (&str, Option<&str>) {
    let search_from = usize::from(spec.starts_with('@'));
    match spec[search_from..].find('@') {
        Some(index) => (
            &spec[..search_from + index],
            Some(&spec[search_from + index + 1..]),
        ),
        None => (spec, None),
    }
}

/// npm package names, optionally scoped.  Rejects anything that pnpm could
/// read as an option or a path.
pub fn validate_package_name(package: &str) -> Result<(), DshCliError> {
    let valid_part = |part: &str| {
        part.chars()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-._~".contains(c))
    };
    let valid = match package.strip_prefix('@') {
        Some(scoped) => scoped
            .split_once('/')
            .is_some_and(|(scope, name)| valid_part(scope) && valid_part(name)),
        None => valid_part(package),
    };
    if valid && package.len() <= 214 {
        Ok(())
    } else {
        Err(DshCliError::InvalidPackage(package.to_owned()))
    }
}

pub fn run(command: &PluginCommand, timeout: Duration) -> Result<CliOutcome, DshCliError> {
    let mut process = Command::new(&command.executable);
    process
        .args(&command.arguments)
        .env("ELECTRON_RUN_AS_NODE", "1")
        .env("DSH_HOME", &command.dsh_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(directory) = command.executable.parent() {
        process.current_dir(directory);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        process.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = process.spawn().map_err(DshCliError::Spawn)?;
    let stdout = child.stdout.take().map(spawn_reader);
    let stderr = child.stderr.take().map(spawn_reader);

    let started = Instant::now();
    let status = loop {
        match child.try_wait().map_err(DshCliError::Spawn)? {
            Some(status) => break status,
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(DshCliError::TimedOut(timeout.as_secs()));
            }
            None => std::thread::sleep(Duration::from_millis(200)),
        }
    };
    let mut combined = String::new();
    for reader in [stdout, stderr].into_iter().flatten() {
        if let Ok(text) = reader.join() {
            combined.push_str(&text);
        }
    }
    let output = output_tail(&combined);
    if status.success() {
        Ok(CliOutcome { output })
    } else {
        let subcommand = command
            .arguments
            .get(5)
            .map(|argument| argument.to_string_lossy().into_owned())
            .unwrap_or_default();
        Err(DshCliError::Failed {
            command: subcommand,
            code: status.code(),
            output,
        })
    }
}

fn spawn_reader(mut stream: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stream.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

fn output_tail(text: &str) -> String {
    let lines = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let start = lines.len().saturating_sub(OUTPUT_TAIL_LINES);
    lines[start..]
        .iter()
        .map(|line| redact_tokens(line))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn builds_the_bundled_cli_command_for_an_instance_home() {
        let root = tempdir().unwrap();
        let executable = root.path().join("DeepSeek Harness.exe");
        fs::write(&executable, b"").unwrap();
        fs::create_dir_all(root.path().join("resources")).unwrap();
        fs::write(root.path().join("resources/app.asar"), b"").unwrap();
        let home = root.path().join("instance/home");

        let command = PluginCommand::new(&executable, &home, &["install"]).unwrap();
        let arguments = command
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(arguments[0], "--expose-internals");
        assert!(
            arguments[1].ends_with(
                &[
                    "app.asar",
                    "dsh",
                    "node_modules",
                    "@deepseek-ai",
                    "dsh-desktop-host",
                    "lib",
                    "cli.js"
                ]
                .join(std::path::MAIN_SEPARATOR_STR)
            )
        );
        assert_eq!(
            &arguments[2..],
            ["plugin", "--profile", "desktop", "install"]
        );
        assert_eq!(command.dsh_home, home);
    }

    #[test]
    fn missing_archive_means_cli_unavailable() {
        let root = tempdir().unwrap();
        let executable = root.path().join("DeepSeek Harness.exe");
        assert!(!is_available(&executable));
        assert!(matches!(
            PluginCommand::new(&executable, root.path(), &["install"]),
            Err(DshCliError::Unavailable(_))
        ));
    }

    #[test]
    fn add_arguments_accept_tarballs_and_registry_specs() {
        let root = tempdir().unwrap();
        let tarball = root.path().join("dsh-meme-1.0.0.tgz");
        fs::write(&tarball, b"tgz").unwrap();
        let tarball_text = tarball.to_string_lossy().into_owned();
        let arguments = add_arguments(&[
            tarball_text.as_str(),
            "dsh-meme@1.2.3",
            "@scope/p",
            "@scope/q@^2",
        ])
        .unwrap();
        assert_eq!(arguments[0], tarball_text);
        assert_eq!(
            &arguments[1..],
            ["dsh-meme@1.2.3", "@scope/p", "@scope/q@^2"]
        );
        for invalid in [
            "--global",
            "missing.tgz",
            "dsh-meme@",
            "dsh-meme@1;rm",
            "Upper",
            "../x",
        ] {
            assert!(add_arguments(&[invalid]).is_err(), "{invalid}");
        }
        assert!(add_arguments(&[]).is_err());
    }

    #[test]
    fn package_names_are_validated_before_reaching_pnpm() {
        for valid in ["dsh-meme", "@scope/plugin", "a.b_c~d"] {
            assert!(validate_package_name(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "", "--global", "../x", "@scope", "@/x", "Upper", "a b", ".hidden",
        ] {
            assert!(validate_package_name(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn output_tail_keeps_last_lines_and_redacts_tokens() {
        let text = (0..50)
            .map(|index| format!("line {index}"))
            .chain(["dsh web: http://127.0.0.1:1/?token=abc".to_owned()])
            .collect::<Vec<_>>()
            .join("\n");
        let tail = output_tail(&text);
        assert_eq!(tail.lines().count(), OUTPUT_TAIL_LINES);
        assert!(tail.ends_with("?token=***"));
        assert!(!tail.contains("line 0\n"));
    }
}
