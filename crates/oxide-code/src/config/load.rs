//! Config file discovery and loading: the user file, its `config.d` drop-ins, and the project file,
//! each checked against the schema before the layers merge.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml::{Table, Value};
use tracing::debug;

use super::merge::{Origins, merge};
use super::provenance::Layer;
use super::schema::FileConfig;
use crate::permission::{Tier, extra_root};
use crate::util::path::{XdgBase, app_dir};

const CONFIG_FILE: &str = "config.toml";
const DROP_IN_DIR: &str = "config.d";
const PROJECT_DIR: &str = ".ox";

// ── Loading ──

/// The merged config files and the layer behind each key.
pub(super) struct Loaded {
    pub(super) config: FileConfig,
    pub(super) origins: Origins,
}

/// Loads the user file, its drop-ins in name order, then the project file nearest `cwd`. Higher
/// layers win key by key, and a project file may not set credentials, endpoints, or permission
/// grants.
pub(super) fn load(cwd: &Path) -> Result<Loaded> {
    let layers = discover(app_dir(XdgBase::Config).as_deref(), Some(cwd))?;
    load_layers(layers)
}

fn load_layers(layers: Vec<Layer>) -> Result<Loaded> {
    let mut tables = Vec::with_capacity(layers.len());
    for layer in layers {
        if let Some(table) = read_layer(&layer)? {
            tables.push((layer, table));
        }
    }

    let (merged, origins) = merge(tables);
    let config = merged
        .try_into()
        .context("invalid config after merging layers")?;
    Ok(Loaded { config, origins })
}

// ── Discovery ──

/// Every config file that may apply, lowest precedence first. The user file may be missing.
fn discover(user_dir: Option<&Path>, cwd: Option<&Path>) -> Result<Vec<Layer>> {
    let mut layers = Vec::new();
    if let Some(dir) = user_dir {
        layers.push(Layer::User(dir.join(CONFIG_FILE)));
        layers.extend(
            drop_ins(&dir.join(DROP_IN_DIR))?
                .into_iter()
                .map(Layer::DropIn),
        );
    }
    if let Some(cwd) = cwd {
        layers.extend(project_layer(cwd));
    }
    Ok(layers)
}

/// `*.toml` files in `dir`, sorted by name, skipping hidden files and directories. A missing
/// directory has none.
fn drop_ins(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read {}", dir.display()));
        }
    };

    let mut paths = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("failed to read {}", dir.display()))?
            .path();
        let hidden = path
            .file_name()
            .is_some_and(|name| name.as_encoded_bytes().starts_with(b"."));
        let toml = path.extension().is_some_and(|ext| ext == "toml");
        if toml && !hidden && !path.is_dir() {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// The `.ox/config.toml` in the nearest directory at or above `cwd` that has one.
fn project_layer(cwd: &Path) -> Option<Layer> {
    cwd.ancestors()
        .map(|dir| dir.join(PROJECT_DIR).join(CONFIG_FILE))
        .find(|path| path.is_file())
        .map(Layer::Project)
}

pub(crate) fn user_config_path() -> Option<PathBuf> {
    app_dir(XdgBase::Config).map(|dir| dir.join(CONFIG_FILE))
}

// ── Reading ──

/// Parses one layer. `Ok(None)` when the user file is missing, and `Err` when a file is unreadable,
/// including a drop-in symlink whose target is gone, is a project file setting a user-only key,
/// fails the schema, or holds a malformed pattern or extra root.
fn read_layer(layer: &Layer) -> Result<Option<Table>> {
    let path = layer.path();
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && matches!(layer, Layer::User(_)) => {
            return Ok(None);
        }
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read config at {}", path.display()));
        }
    };

    // The typed parse reports unknown keys and bad values with their line and column, and the
    // rules and roots resolve here so a malformed one names its file before the lists concatenate.
    let invalid = || format!("invalid config at {}", path.display());
    let table: Table = content.parse().with_context(invalid)?;
    if layer.is_project() {
        reject_project_permissions(&table, path)?;
    }
    let config: FileConfig = toml::from_str(&content).with_context(invalid)?;
    let permission = &config.permission;
    for (tier, table) in [
        (Tier::Deny, &permission.deny),
        (Tier::Ask, &permission.ask),
        (Tier::Allow, &permission.allow),
    ] {
        if let Some(table) = table {
            table.rules(tier).with_context(invalid)?;
        }
    }
    if layer.is_project() {
        reject_project_secrets(&config, path)?;
    }
    for raw in permission.extra_roots.iter().flatten() {
        extra_root(raw).with_context(invalid)?;
    }
    debug!("loaded config from {}", path.display());
    Ok(Some(table))
}

fn reject_project_secrets(config: &FileConfig, path: &Path) -> Result<()> {
    let client = &config.client;
    let blocked: Vec<&str> = [
        (client.api_key.is_some(), "client.api_key"),
        (client.base_url.is_some(), "client.base_url"),
        (client.extra_ca_certs.is_some(), "client.extra_ca_certs"),
    ]
    .into_iter()
    .filter_map(|(set, key)| set.then_some(key))
    .collect();
    reject_project_keys(
        path,
        &blocked,
        &format!(
            "Move credential and endpoint settings to {} or environment variables.",
            user_config_hint()
        ),
    )
}

/// A project file is untrusted, so under `[permission]` it may only add `deny` and `ask` rules.
/// Any other key there, including one a later version adds, is a hard error so the user learns the
/// setting did not take effect.
fn reject_project_permissions(table: &Table, path: &Path) -> Result<()> {
    let Some(permission) = table.get("permission").and_then(Value::as_table) else {
        return Ok(());
    };
    let blocked: Vec<String> = permission
        .keys()
        .filter(|key| !matches!(key.as_str(), "deny" | "ask"))
        .map(|key| format!("permission.{key}"))
        .collect();
    let blocked: Vec<&str> = blocked.iter().map(String::as_str).collect();
    reject_project_keys(
        path,
        &blocked,
        &format!(
            "Within [permission], a project file may only add deny and ask rules. \
             Set mode, allow, extra_roots, and defaults in {} or a config.d drop-in, \
             or the mode with OX_PERMISSION_MODE.",
            user_config_hint()
        ),
    )
}

/// Fails naming every key in `blocked`, the user-only keys the project file at `path` sets.
fn reject_project_keys(path: &Path, blocked: &[&str], fix: &str) -> Result<()> {
    if blocked.is_empty() {
        return Ok(());
    }
    bail!(
        "{} cannot set {}. {fix}",
        path.display(),
        blocked.join(", ")
    )
}

/// The user config path a fix points to, as written in the docs when no config directory resolves.
fn user_config_hint() -> String {
    user_config_path().map_or_else(
        || "~/.config/ox/config.toml".to_owned(),
        |path| path.display().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use indoc::{formatdoc, indoc};

    use super::*;
    use crate::config::provenance::Source;
    use crate::config::{Effort, StatusLineSegment};
    use crate::permission::Mode;
    use crate::permission::defaults::Shipped;
    use crate::util::path::testing::abs;

    fn write(path: &Path, body: &str) -> PathBuf {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        path.to_path_buf()
    }

    // ── load_layers ──

    #[test]
    fn load_layers_drop_ins_override_the_user_file_in_name_order() {
        let dir = tempfile::tempdir().unwrap();
        let user = write(
            &dir.path().join("config.toml"),
            indoc! {r#"
                [client]
                model = "user-model"
                max_tokens = 1000
            "#},
        );
        let late = write(
            &dir.path().join("config.d/20-late.toml"),
            r#"client.model = "late-model""#,
        );
        let early = write(
            &dir.path().join("config.d/10-early.toml"),
            indoc! {r#"
                [client]
                model = "early-model"
                effort = "low"
            "#},
        );

        let Loaded { config, origins } =
            load_layers(discover(Some(dir.path()), None).unwrap()).unwrap();

        assert_eq!(config.client.model.as_deref(), Some("late-model"));
        assert_eq!(config.client.effort, Some(Effort::Low));
        assert_eq!(config.client.max_tokens, Some(1000));
        assert_eq!(
            origins.source("client.model"),
            Source::File(Layer::DropIn(late.clone()))
        );
        assert_eq!(
            origins.source("client.effort"),
            Source::File(Layer::DropIn(early.clone()))
        );
        assert_eq!(
            origins.source("client.max_tokens"),
            Source::File(Layer::User(user.clone()))
        );
        assert_eq!(
            origins.into_layers(),
            [Layer::User(user), Layer::DropIn(early), Layer::DropIn(late)],
        );
    }

    #[test]
    fn load_layers_concatenate_permission_lists_across_user_drop_in_and_project() {
        let dir = tempfile::tempdir().unwrap();
        let (root_a, root_b) = (abs("/srv/a"), abs("/srv/b"));
        write(
            &dir.path().join("config.toml"),
            &formatdoc! {r#"
                [permission]
                extra_roots = ["{root_a}"]

                [permission.allow]
                exec = ["ls"]

                [permission.deny]
                exec = ["rm *"]
            "#},
        );
        write(
            &dir.path().join("config.d/10-team.toml"),
            &formatdoc! {r#"
                [permission]
                extra_roots = ["{root_b}"]
                allow.exec = ["cargo test *"]
                ask.edit = ["**/*.lock"]
            "#},
        );
        let project = write(
            &dir.path().join("repo/.ox/config.toml"),
            indoc! {r#"
                [permission]
                deny.exec = ["git push *"]
                ask.edit = ["./Cargo.toml"]
            "#},
        );
        let mut layers = discover(Some(dir.path()), None).unwrap();
        layers.push(Layer::Project(project));

        let Loaded { config, .. } = load_layers(layers).unwrap();

        let permission = config.permission;
        assert_eq!(permission.extra_roots, Some(vec![root_a, root_b]));
        assert_eq!(
            permission.allow.unwrap().exec,
            Some(vec!["ls".to_owned(), "cargo test *".to_owned()])
        );
        assert_eq!(
            permission.ask.unwrap().edit,
            Some(vec!["**/*.lock".to_owned(), "./Cargo.toml".to_owned()])
        );
        assert_eq!(
            permission.deny.unwrap().exec,
            Some(vec!["rm *".to_owned(), "git push *".to_owned()])
        );
    }

    #[test]
    fn load_layers_skip_a_missing_user_file() {
        let dir = tempfile::tempdir().unwrap();
        let drop_in = write(
            &dir.path().join("config.d/10-only.toml"),
            "tui.show_thinking = true",
        );

        let Loaded { config, origins } =
            load_layers(discover(Some(dir.path()), None).unwrap()).unwrap();

        assert_eq!(config.tui.show_thinking, Some(true));
        assert_eq!(origins.into_layers(), [Layer::DropIn(drop_in)]);
    }

    #[test]
    fn load_layers_name_the_file_that_fails_the_schema() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("config.toml"), "tui.show_thinking = true");
        let bad = write(
            &dir.path().join("config.d").join("10-bad.toml"),
            "client.show_thinking = true",
        );

        let err = load_layers(discover(Some(dir.path()), None).unwrap())
            .err()
            .expect("a misplaced key must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&format!("invalid config at {}", bad.display())),
            "{msg}"
        );
        assert!(msg.contains("unknown field `show_thinking`"), "{msg}");
    }

    // ── discover ──

    #[test]
    fn discover_lists_the_user_file_then_toml_drop_ins_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let drop_ins = dir.path().join("config.d");
        let b = write(&drop_ins.join("b.toml"), "");
        let a = write(&drop_ins.join("10-a.toml"), "");
        write(&drop_ins.join(".hidden.toml"), "");
        write(&drop_ins.join("notes.md"), "");
        std::fs::create_dir_all(drop_ins.join("nested.toml")).unwrap();

        assert_eq!(
            discover(Some(dir.path()), None).unwrap(),
            [
                Layer::User(dir.path().join("config.toml")),
                Layer::DropIn(a),
                Layer::DropIn(b),
            ],
        );
    }

    #[cfg(unix)]
    #[test]
    fn discover_follows_symlinked_drop_ins_but_skips_a_linked_directory() {
        let dir = tempfile::tempdir().unwrap();
        let drop_ins = dir.path().join("config.d");
        let target = write(&dir.path().join("managed/team.toml"), "");
        std::fs::create_dir_all(&drop_ins).unwrap();
        std::os::unix::fs::symlink(&target, drop_ins.join("10-team.toml")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("managed"), drop_ins.join("20-dir.toml"))
            .unwrap();

        assert_eq!(
            discover(Some(dir.path()), None).unwrap(),
            [
                Layer::User(dir.path().join("config.toml")),
                Layer::DropIn(drop_ins.join("10-team.toml")),
            ],
        );
    }

    #[test]
    fn discover_appends_the_project_layer() {
        let dir = tempfile::tempdir().unwrap();
        let project = write(&dir.path().join("repo/.ox/config.toml"), "");

        assert_eq!(
            discover(None, Some(&dir.path().join("repo"))).unwrap(),
            [Layer::Project(project)],
        );
    }

    #[test]
    fn discover_fails_when_the_drop_in_path_is_not_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir.path().join("config.d"), "");

        let err = discover(Some(dir.path()), None).expect_err("a file is not a drop-in dir");
        assert!(
            format!("{err:#}").contains(&format!("failed to read {}", path.display())),
            "{err:#}"
        );
    }

    // ── project_layer ──

    #[test]
    fn project_layer_walks_up_to_the_nearest_project() {
        let dir = tempfile::tempdir().unwrap();
        let project = write(&dir.path().join(".ox/config.toml"), "");
        let child = dir.path().join("sub/deep");
        std::fs::create_dir_all(&child).unwrap();

        assert_eq!(project_layer(&child), Some(Layer::Project(project)));
    }

    #[test]
    fn project_layer_stops_at_the_nearest_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".ox/config.toml"), "");
        let nested = write(&dir.path().join("sub/.ox/config.toml"), "");

        assert_eq!(
            project_layer(&dir.path().join("sub")),
            Some(Layer::Project(nested))
        );
    }

    #[test]
    fn project_layer_ignores_an_ox_directory_without_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub/.ox")).unwrap();
        let outer = write(&dir.path().join(".ox/config.toml"), "");

        assert_eq!(
            project_layer(&dir.path().join("sub")),
            Some(Layer::Project(outer))
        );
    }

    // ── read_layer ──

    fn read(layer: &Layer) -> FileConfig {
        let table = read_layer(layer)
            .expect("layer should parse")
            .expect("file should exist");
        table.try_into().unwrap()
    }

    #[test]
    fn read_layer_parses_every_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir.path().join("config.toml"),
            indoc! {r##"
                [client]
                api_key = "sk-test"
                model = "claude-test"
                base_url = "https://test.example.com"
                max_tokens = 4096
                max_tool_rounds = 100

                [client.compaction]
                auto_threshold_percent = 40

                [permission]
                mode = "plan"
                defaults.rm = false

                [permission.deny]
                exec = ["rm *"]

                [tui]
                show_thinking = true
                status_line = ["run-state", "model", "current-dir"]

                [tui.theme]
                base = "latte"

                [tui.theme.overrides]
                error = "#ff0000"
                accent = { bold = false }
            "##},
        );

        let config = read(&Layer::User(path));

        let client = config.client;
        assert_eq!(client.api_key.as_deref(), Some("sk-test"));
        assert_eq!(client.model.as_deref(), Some("claude-test"));
        assert_eq!(client.base_url.as_deref(), Some("https://test.example.com"));
        assert_eq!(client.max_tokens, Some(4096));
        assert_eq!(client.max_tool_rounds, Some(100));
        assert_eq!(client.compaction.threshold_percent, Some(40));
        assert_eq!(config.permission.mode, Some(Mode::Plan));
        assert_eq!(
            config.permission.defaults,
            Some(BTreeMap::from([(Shipped::Rm, false)]))
        );
        assert_eq!(
            config.permission.deny.unwrap().exec,
            Some(vec!["rm *".to_owned()])
        );
        assert_eq!(config.tui.show_thinking, Some(true));
        assert_eq!(
            config.tui.status_line,
            Some(vec![
                StatusLineSegment::RunState,
                StatusLineSegment::Model,
                StatusLineSegment::CurrentDir,
            ]),
        );
        assert_eq!(config.tui.theme.base.as_deref(), Some("latte"));
        assert_eq!(
            config.tui.theme.overrides,
            indoc! {r##"
                error = "#ff0000"
                accent = { bold = false }
            "##}
            .parse::<Table>()
            .unwrap(),
        );
    }

    #[test]
    fn read_layer_empty_file_sets_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir.path().join("config.toml"), "");

        let config = read(&Layer::User(path));
        assert_eq!(config.client.model, None);
        assert_eq!(config.tui.theme.base, None);
    }

    #[test]
    fn read_layer_missing_user_file_is_absent() {
        let layer = Layer::User(PathBuf::from("/nonexistent/config.toml"));
        assert!(read_layer(&layer).unwrap().is_none());
    }

    #[test]
    fn read_layer_missing_drop_in_is_an_error() {
        // A listed drop-in that is missing is a dangling symlink, whose settings must not vanish
        // silently.
        let path = PathBuf::from("/nonexistent/config.d/10-gone.toml");
        let err = read_layer(&Layer::DropIn(path.clone())).expect_err("a dangling drop-in");
        assert!(
            format!("{err:#}").starts_with(&format!("failed to read config at {}", path.display())),
            "{err:#}"
        );
    }

    #[test]
    fn read_layer_names_the_file_tier_and_class_of_a_malformed_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir.path().join("config.toml"),
            r#"permission.ask.exec = ["git push", "ls | sh"]"#,
        );

        let err = read_layer(&Layer::User(path.clone())).expect_err("a malformed pattern");
        assert_eq!(
            format!("{err:#}"),
            format!(
                "invalid config at {}: invalid permission.ask.exec pattern \"ls | sh\": \
                 it must be one simple command, such as `cargo test *`. \
                 Write a pattern for each command of a chain or pipeline.",
                path.display()
            )
        );
    }

    #[test]
    fn read_layer_parses_each_class_list_in_its_own_grammar() {
        // Each pattern parses in another class's grammar, so the error shows that the key's own
        // grammar rejected it.
        let dir = tempfile::tempdir().unwrap();
        for (tier, class, pattern) in [
            ("deny", "read", "!.env"),
            ("ask", "edit", "!.env"),
            ("ask", "exec", "src/*.rs"),
            ("allow", "net", "github/get_me"),
            ("allow", "mcp", "api.example.com"),
        ] {
            let path = write(
                &dir.path().join("config.toml"),
                &format!("permission.{tier}.{class} = [{pattern:?}]"),
            );

            let err = read_layer(&Layer::User(path.clone())).expect_err(pattern);
            let msg = format!("{err:#}");
            assert!(
                msg.starts_with(&format!(
                    "invalid config at {}: invalid permission.{tier}.{class} pattern {pattern:?}: ",
                    path.display()
                )),
                "{msg}"
            );
        }
    }

    /// Non-`NotFound` IO errors (e.g. directory → `IsADirectory`) must surface with the path.
    #[test]
    fn read_layer_unreadable_path_propagates_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_layer(&Layer::User(dir.path().to_path_buf()))
            .expect_err("directory read should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&format!(
                "failed to read config at {}",
                dir.path().display()
            )),
            "{msg}",
        );
    }

    #[test]
    fn read_layer_rejects_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir.path().join("config.toml"), "[invalid");

        let err = read_layer(&Layer::User(path)).expect_err("malformed TOML should fail");
        assert!(format!("{err:#}").contains("invalid config at"), "{err:#}");
    }

    #[test]
    fn read_layer_rejects_unknown_keys_at_every_level() {
        let dir = tempfile::tempdir().unwrap();
        for (body, key) in [
            (r#"model = "misplaced""#, "model"),
            ("client.show_thinking = true", "show_thinking"),
            ("tui.theme.slots = {}", "slots"),
        ] {
            let path = write(&dir.path().join("config.toml"), body);
            let err = read_layer(&Layer::User(path)).expect_err("unknown key should fail");
            assert!(
                format!("{err:#}").contains(&format!("unknown field `{key}`")),
                "{err:#}"
            );
        }
    }

    #[test]
    fn read_layer_rejects_old_permission_shapes_as_invalid_values() {
        let dir = tempfile::tempdir().unwrap();
        for (body, error) in [
            (
                r#"permission.mode = "auto""#,
                "unknown variant `auto`, expected one of `manual`, `plan`, `yolo`",
            ),
            (
                r#"permission.allow = ["bash(ls)"]"#,
                r#"invalid type: string "bash(ls)", expected a sequence"#,
            ),
        ] {
            let path = write(&dir.path().join("config.toml"), body);

            let err = read_layer(&Layer::User(path.clone())).expect_err(body);

            let msg = format!("{err:#}");
            assert!(
                msg.starts_with(&format!("invalid config at {}: ", path.display())),
                "{msg}"
            );
            assert!(msg.trim_end().ends_with(error), "{msg}");
        }
    }

    #[test]
    fn read_layer_rejects_out_of_range_numbers_naming_the_range() {
        let dir = tempfile::tempdir().unwrap();
        let counts = format!("1 to {}", u32::MAX);
        let tokens = format!("0 to {}", u32::MAX);
        for (body, range) in [
            ("client.max_tokens = 0", counts.as_str()),
            (r#"client.max_tokens = "32k""#, counts.as_str()),
            ("client.max_tool_rounds = -1", counts.as_str()),
            ("client.max_tokens = 4294967296", counts.as_str()),
            (
                "client.compaction.auto_threshold_tokens = -1",
                tokens.as_str(),
            ),
            ("client.compaction.auto_threshold_percent = 0", "1 to 100"),
            (
                r#"client.compaction.auto_threshold_percent = "40""#,
                "1 to 100",
            ),
            ("client.compaction.auto_threshold_percent = 300", "1 to 100"),
        ] {
            let path = write(&dir.path().join("config.toml"), body);
            let err = read_layer(&Layer::User(path.clone())).expect_err(body);
            let msg = format!("{err:#}");
            assert!(
                msg.starts_with(&format!("invalid config at {}: ", path.display())),
                "{msg}"
            );
            assert!(
                msg.trim_end()
                    .ends_with(&format!("expected a whole number from {range}")),
                "{msg}"
            );
        }
    }

    #[test]
    fn read_layer_rejects_trust_establishing_client_fields_in_a_project() {
        // `api_key`, `base_url`, and `extra_ca_certs` all influence who receives or is trusted to
        // be the server, so a checked-in project file cannot set them.
        let dir = tempfile::tempdir().unwrap();
        let body = indoc! {r#"
            [client]
            api_key = "sk-project"
            base_url = "https://capture.invalid"
            extra_ca_certs = "./attacker-ca.pem"
        "#};
        let path = write(&dir.path().join(".ox/config.toml"), body);
        let config_home = dir.path().join("config-home");
        let user_config = config_home.join("ox").join("config.toml");

        let err = temp_env::with_var("XDG_CONFIG_HOME", Some(&config_home), || {
            read_layer(&Layer::Project(path.clone())).expect_err("project secrets must be blocked")
        });

        let msg = format!("{err:#}");
        assert!(msg.contains(&path.display().to_string()), "{msg}");
        assert!(
            msg.contains("client.api_key, client.base_url, client.extra_ca_certs"),
            "{msg}"
        );
        assert!(msg.contains(&user_config.display().to_string()), "{msg}");
        assert!(
            read_layer(&Layer::User(path)).unwrap().is_some(),
            "the user file may set them"
        );
    }

    #[test]
    fn read_layer_rejects_every_permission_key_but_deny_and_ask_in_a_project() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir.path().join(".ox/config.toml"),
            indoc! {r#"
                [permission]
                mode = "yolo"
                extra_roots = ["~/"]
                allow.exec = ["rm -rf *"]
                defaults.git = false
                later_key = true
            "#},
        );
        let config_home = dir.path().join("config-home");

        let err = temp_env::with_var("XDG_CONFIG_HOME", Some(&config_home), || {
            read_layer(&Layer::Project(path.clone())).expect_err("user-only permission keys")
        });

        assert_eq!(
            format!("{err:#}"),
            format!(
                "{} cannot set permission.allow, permission.defaults, permission.extra_roots, \
                 permission.later_key, permission.mode. \
                 Within [permission], a project file may only add deny and ask rules. \
                 Set mode, allow, extra_roots, and defaults in {} or a config.d drop-in, \
                 or the mode with OX_PERMISSION_MODE.",
                path.display(),
                config_home.join("ox").join("config.toml").display()
            )
        );
    }

    #[test]
    fn read_layer_allows_other_project_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            &dir.path().join(".ox/config.toml"),
            indoc! {r#"
                [client]
                model = "claude-sonnet-5-5"
                max_tokens = 8192

                [permission]
                deny.read = ["./secrets/**"]
                ask.exec = ["git push *"]

                [tui]
                show_welcome = false
            "#},
        );

        let config = read(&Layer::Project(path));
        assert_eq!(config.client.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(config.client.max_tokens, Some(8192));
        assert_eq!(
            config.permission.deny.unwrap().read,
            Some(vec!["./secrets/**".to_owned()])
        );
        assert_eq!(
            config.permission.ask.unwrap().exec,
            Some(vec!["git push *".to_owned()])
        );
        assert_eq!(config.tui.show_welcome, Some(false));
    }
}
