//! Stand-in for the DSH Electron desktop app, used by
//! `tests/process_integration.rs`.
//!
//! It receives the same arguments and environment Marina gives the real app
//! (`--user-data-dir=...`, `DSH_HOME`, `DSH_DESKTOP_DIAGNOSTIC_FILE`). The
//! behaviour comes from `<DSH_HOME>/fake-mode.txt`, else `FAKE_DESKTOP_MODE`,
//! else `run`:
//!
//! - `run`: listen on 127.0.0.1, print the `dsh web:` line, serve HTTP 200.
//! - `crash`: exit with code 1 after about one second.
//! - `diagnostic`: write an error stack to the diagnostic file, exit with code 1.
//! - `spawn-child`: start a `run` copy of itself, write its pid to
//!   `<DSH_HOME>/child.pid`, then stay alive.
//! - `hang`: stay alive without doing anything.

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::Duration;

/// Set on the child started by `spawn-child`. It wins over `fake-mode.txt`,
/// which the child shares with its parent through `DSH_HOME`.
const FORCE_MODE_VARIABLE: &str = "FAKE_DESKTOP_FORCE_MODE";
const MODE_VARIABLE: &str = "FAKE_DESKTOP_MODE";

const DIAGNOSTIC_TEXT: &str = "Uncaught Exception:
Error: fake-desktop simulated startup failure
    at loadPlugin (C:\\fake\\resources\\app.asar\\dist\\main.js:42:13)
    at App.<anonymous> (C:\\fake\\resources\\app.asar\\dist\\main.js:7:3)
    at App.emit (node:events:518:28)
";

fn main() -> ExitCode {
    let home = env::var_os("DSH_HOME").map(PathBuf::from);
    match mode(home.as_deref()).as_str() {
        "run" => run(),
        "crash" => {
            thread::sleep(Duration::from_secs(1));
            ExitCode::from(1)
        }
        "diagnostic" => diagnostic(),
        "spawn-child" => spawn_child(home.as_deref()),
        "hang" => sleep_forever(),
        other => {
            eprintln!("fake-desktop: unknown mode {other:?}");
            ExitCode::from(2)
        }
    }
}

fn mode(home: Option<&Path>) -> String {
    if let Ok(mode) = env::var(FORCE_MODE_VARIABLE) {
        return mode;
    }
    if let Some(text) = home.and_then(|home| fs::read_to_string(home.join("fake-mode.txt")).ok()) {
        let mode = text.trim_start_matches('\u{feff}').trim();
        if !mode.is_empty() {
            return mode.to_owned();
        }
    }
    env::var(MODE_VARIABLE).unwrap_or_else(|_| "run".to_owned())
}

fn run() -> ExitCode {
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("fake-desktop: cannot listen: {error}");
            return ExitCode::from(3);
        }
    };
    let port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            eprintln!("fake-desktop: cannot read the listening address: {error}");
            return ExitCode::from(3);
        }
    };
    {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "dsh web: http://127.0.0.1:{port}/?token=secret123");
        let _ = stdout.flush();
    }
    for stream in listener.incoming().flatten() {
        thread::spawn(move || respond(stream));
    }
    sleep_forever()
}

fn respond(mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") && request.len() < 64 * 1024 {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => request.extend_from_slice(&buffer[..read]),
        }
    }
    let body = b"ok";
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(body);
    let _ = stream.shutdown(Shutdown::Both);
}

fn diagnostic() -> ExitCode {
    let Some(path) = env::var_os("DSH_DESKTOP_DIAGNOSTIC_FILE").map(PathBuf::from) else {
        eprintln!("fake-desktop: DSH_DESKTOP_DIAGNOSTIC_FILE is not set");
        return ExitCode::from(2);
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(error) = fs::write(&path, DIAGNOSTIC_TEXT) {
        eprintln!("fake-desktop: cannot write {}: {error}", path.display());
        return ExitCode::from(2);
    }
    ExitCode::from(1)
}

fn spawn_child(home: Option<&Path>) -> ExitCode {
    let Some(home) = home else {
        eprintln!("fake-desktop: DSH_HOME is not set");
        return ExitCode::from(2);
    };
    let executable = match env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            eprintln!("fake-desktop: cannot locate itself: {error}");
            return ExitCode::from(2);
        }
    };
    // No `--user-data-dir`, so adoption scans only ever match the parent.
    let child = match Command::new(executable)
        .env(FORCE_MODE_VARIABLE, "run")
        .stdin(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("fake-desktop: cannot spawn the child: {error}");
            return ExitCode::from(2);
        }
    };
    // Write then rename so a reader never sees a partial pid.
    let staging = home.join("child.pid.tmp");
    if let Err(error) = fs::write(&staging, child.id().to_string())
        .and_then(|()| fs::rename(&staging, home.join("child.pid")))
    {
        eprintln!("fake-desktop: cannot write child.pid: {error}");
    }
    // Dropping `Child` neither waits for nor kills the child.
    drop(child);
    sleep_forever()
}

fn sleep_forever() -> ! {
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}
