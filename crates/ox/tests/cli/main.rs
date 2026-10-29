//! End-to-end tests of the `ox` binary: startup, `ox config check`, credential discovery, the
//! headless and REPL drivers, session listing and resume, and signal exits.

#[path = "../support/env.rs"]
mod env;

mod config_check;
mod headless;
mod native_auth;
mod repl;
mod sessions;
#[cfg(unix)]
mod signals;
mod startup;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use indoc::{formatdoc, indoc};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use ox_core::engine::testing::{mount_reply, mount_tool_call_once, request_bodies};
use ox_core::session::resolver::export_session;
use ox_core::session::store::SessionStore;
use ox_core::session::store::listing::{ArchiveState, ListScope, ProjectScope, SessionInfo};
use ox_core::util::git::testing::{isolate_git, isolated_git};
use ox_core::util::tls::testing::TEST_CA_PATH;

fn ox(root: &Path) -> Command {
    let mut ox = Command::new(env!("CARGO_BIN_EXE_ox"));
    env::remove_inherited(&mut ox, |name| {
        env::is_provider_var(name)
            || env::is_ox_setting(name)
            || matches!(name, "RUST_BACKTRACE" | "RUST_LOG")
    });
    isolate_git(&mut ox);
    env::use_scratch_dirs(&mut ox, root);
    // On Linux, building the HTTP client reads the system trust store and fails when it holds no
    // certificates, so the tests pin it to a test CA.
    ox.env("SSL_CERT_FILE", TEST_CA_PATH);
    // Tokio's `output` inherits an unset stdin where std's closes it, and `--print` reads a
    // non-terminal stdin to its end, so a parent stdin that never closes would hang the run.
    ox.stdin(Stdio::null());
    ox
}

/// Builds an `ox config check` run from `work/` under `root`, created empty when missing.
fn config_check(root: &Path) -> Command {
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("the scratch work directory can be created");
    let mut command = ox(root);
    command.args(["config", "check"]).current_dir(work);
    command
}

/// Writes `body` to the user config file under `root` and returns the file's path.
fn write_user_config(root: &Path, body: &str) -> PathBuf {
    let dir = root.join("config").join("ox");
    std::fs::create_dir_all(&dir).expect("the scratch config directory can be created");
    let file = dir.join("config.toml");
    std::fs::write(&file, body).expect("the scratch config file can be written");
    file
}

/// Every path under `root`, sorted.
fn tree(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("the scratch directory can be listed") {
            let path = entry.expect("the scratch entry can be read").path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

fn assert_private_auth_run(root: &Path, output: &Output) {
    let secrets = [
        "private-token-sentinel",
        "private-header-sentinel",
        "987654321",
    ];
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        for secret in secrets {
            assert!(!text.contains(secret), "{text}");
        }
    }
    for dir in [root.join("data"), root.join("state")] {
        if !dir.exists() {
            continue;
        }
        for path in tree(&dir).into_iter().filter(|path| path.is_file()) {
            let bytes = std::fs::read(&path).expect("the run's artifact can be read");
            let text = String::from_utf8_lossy(&bytes);
            for secret in secrets {
                assert!(!text.contains(secret), "{}: {text}", path.display());
            }
        }
    }
    // A byte scan of the database can miss a value split across its pages.
    if !root.join("data").join("ox").join("sessions.db").exists() {
        return;
    }
    for export in saved_exports(root) {
        for secret in secrets {
            assert!(!export.contains(secret), "{export}");
        }
    }
}

/// The session store a run under `root` writes.
fn saved_store(root: &Path) -> SessionStore {
    SessionStore::open_at(root.join("data").join("ox"), "cli").expect("the run's store opens")
}

/// Every unarchived session the runs under `root` saved, in any project.
fn saved_sessions(root: &Path) -> Vec<SessionInfo> {
    let scope = ListScope {
        projects: ProjectScope::Every,
        archive: ArchiveState::Active,
    };
    saved_store(root)
        .list_paged(None, scope)
        .expect("the run's sessions list")
        .sessions()
        .to_vec()
}

/// The export of each session [`saved_sessions`] lists.
fn saved_exports(root: &Path) -> Vec<String> {
    let store = saved_store(root);
    saved_sessions(root)
        .iter()
        .map(|info| {
            let mut out = Vec::new();
            export_session(&store, &info.session_id, &mut out).expect("the session exports");
            String::from_utf8(out).expect("an export is UTF-8")
        })
        .collect()
}

/// The records of the `ox export` of session `id` from the store under `root`.
fn exported(root: &Path, id: &str) -> Vec<serde_json::Value> {
    let output = ox(root)
        .args(["export", id])
        .current_dir(root)
        .output()
        .expect("ox export runs");
    assert!(output.status.success(), "{output:?}");
    output
        .stdout
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("an export line is JSON"))
        .collect()
}

fn request_header<'a>(request: &'a wiremock::Request, name: &str) -> Option<&'a str> {
    request
        .headers
        .get(name)
        .map(|value| value.to_str().expect("the request header is text"))
}
