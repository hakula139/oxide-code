//! Config file discovery and loading: the user file, its `config.d` drop-ins, and the project file,
//! each checked against the schema before the layers merge.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use toml::{Table, Value};
use tracing::debug;

use super::merge::{Origins, merge};
use super::provenance::Layer;
use super::schema::FileConfig;
use crate::permission::project::{ProjectFileError, resolve_project_file};
use crate::permission::{Tier, extra_root};
use crate::util::path::{XdgBase, app_dir};
use crate::util::toml::{describe_error, locate_error};

const CONFIG_FILE: &str = "config.toml";
const DROP_IN_DIR: &str = "config.d";
const PROJECT_DIR: &str = ".ox";

// ── Loading ──

/// The merged config files and the layer behind each key.
pub(super) struct Loaded {
    pub(super) config: FileConfig,
    pub(super) origins: Origins,
}

/// Loads the user file, its drop-ins in name order, then the project file nearest `cwd` within
/// `project_root`. Later layers win per key, and a project file may not set credentials,
/// endpoints, or permission grants.
pub(super) fn load(cwd: &Path, project_root: &Path) -> Result<Loaded> {
    let user_dir = app_dir(XdgBase::Config);
    let layers = discover(user_dir.as_deref(), Some((cwd, project_root)))?;
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

/// Every config file that may apply, lowest precedence first. The user file may be missing, and
/// `project` pairs the working directory with its project root.
fn discover(user_dir: Option<&Path>, project: Option<(&Path, &Path)>) -> Result<Vec<Layer>> {
    let mut layers = Vec::new();
    if let Some(dir) = user_dir {
        layers.push(Layer::User(dir.join(CONFIG_FILE)));
        layers.extend(
            drop_ins(&dir.join(DROP_IN_DIR))?
                .into_iter()
                .map(Layer::DropIn),
        );
    }
    if let Some((cwd, root)) = project {
        layers.extend(project_layer(cwd, root)?);
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

/// The `.ox/config.toml` in the nearest directory from `cwd` up to `root` that has one. It must be
/// a file [`resolve_project_file`] lets the checkout supply.
fn project_layer(cwd: &Path, root: &Path) -> Result<Option<Layer>> {
    let Some(path) = cwd
        .ancestors()
        .take_while(|dir| dir.starts_with(root))
        .map(|dir| dir.join(PROJECT_DIR).join(CONFIG_FILE))
        .find(|path| path.is_file())
    else {
        return Ok(None);
    };

    let fix = || {
        format!(
            "Replace the link with a regular file, or move personal settings to {}.",
            user_config_hint()
        )
    };
    match resolve_project_file(&path, root) {
        Ok(_) => Ok(Some(Layer::Project(path))),
        Err(ProjectFileError::Io(e)) => {
            Err(e).with_context(|| format!("failed to resolve config at {}", path.display()))
        }
        Err(ProjectFileError::Outside) => bail!(
            "{} links outside the project root {}. {}",
            path.display(),
            root.display(),
            fix(),
        ),
        Err(ProjectFileError::Linked) => bail!(
            "{} is reached through a symlink, \
             which is not followed when the project root {} contains the home directory. {}",
            path.display(),
            root.display(),
            fix(),
        ),
    }
}

pub fn user_config_path() -> Option<PathBuf> {
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
    // A project file can link to another file in the checkout, and a parser message can quote one
    // of its values, as in `invalid type: string "..."`, so a project error gives only where it is.
    let invalid_toml = |e: toml::de::Error| {
        let detail = if layer.is_project() {
            locate_error(&content, &e)
        } else {
            Some(describe_error(&content, &e))
        };
        detail.map_or_else(
            || anyhow!(invalid()),
            |detail| anyhow!("{}: {detail}", invalid()),
        )
    };
    let table: Table = content.parse().map_err(invalid_toml)?;
    if layer.is_project() {
        reject_project_client(&table, path)?;
        reject_project_permissions(&table, path)?;
    }
    let config: FileConfig = toml::from_str(&content).map_err(invalid_toml)?;
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
    for raw in permission.extra_roots.iter().flatten() {
        extra_root(raw).with_context(invalid)?;
    }
    debug!("loaded config from {}", path.display());
    Ok(Some(table))
}

/// Rejects every `[client]` key a project file, which is untrusted, may not set. The allowed keys
/// tune requests, while the rest, including any added later, can redirect credentials, the
/// endpoint, or TLS trust.
fn reject_project_client(table: &Table, path: &Path) -> Result<()> {
    const ALLOWED: [&str; 6] = [
        "model",
        "effort",
        "max_tokens",
        "max_tool_rounds",
        "prompt_cache_ttl",
        "compaction",
    ];
    reject_project_keys(
        path,
        &keys_outside(table, "client", &ALLOWED),
        &format!(
            "Within [client], a project file may only set model, effort, max_tokens, \
             max_tool_rounds, prompt_cache_ttl, and compaction. \
             Set credentials, endpoints, and trust in {} or environment variables.",
            user_config_hint()
        ),
    )
}

/// Rejects every `[permission]` key but `deny` and `ask` in a project file, which is untrusted.
fn reject_project_permissions(table: &Table, path: &Path) -> Result<()> {
    reject_project_keys(
        path,
        &keys_outside(table, "permission", &["deny", "ask"]),
        &format!(
            "Within [permission], a project file may only add deny and ask rules. \
             Set mode, allow, extra_roots, and defaults in {} or a config.d drop-in, \
             or the mode with OX_PERMISSION_MODE.",
            user_config_hint()
        ),
    )
}

/// The dotted names of the `section` keys in `table` outside `allowed`.
fn keys_outside(table: &Table, section: &str, allowed: &[&str]) -> Vec<String> {
    table
        .get(section)
        .and_then(Value::as_table)
        .into_iter()
        .flat_map(Table::keys)
        .filter(|key| !allowed.contains(&key.as_str()))
        .map(|key| format!("{section}.{key}"))
        .collect()
}

/// Fails naming every key in `blocked`, the user-only keys the project file at `path` sets.
fn reject_project_keys(path: &Path, blocked: &[String], fix: &str) -> Result<()> {
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
pub fn user_config_hint() -> String {
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
    use crate::config::{Effort, PromptCacheTtl, StatusLineSegment};
    use crate::permission::Mode;
    use crate::permission::defaults::Shipped;
    use crate::util::fs::testing::{symlink_dir, symlink_file};
    use crate::util::path::local_testing::abs;
    use crate::util::path::testing::HOME_VAR;

    fn write(path: &Path, body: &str) -> PathBuf {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        path.to_path_buf()
    }

    mod load_layers {
        use super::*;

        #[test]
        fn drop_ins_override_the_user_file_in_name_order() {
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
        fn concatenate_permission_lists_across_user_drop_in_and_project() {
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
        fn skip_a_missing_user_file() {
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
        fn name_the_file_that_fails_the_schema() {
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
    }

    mod discover {
        use super::*;

        #[test]
        fn lists_the_user_file_then_toml_drop_ins_by_name() {
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

        #[test]
        fn appends_the_project_layer() {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path().join("repo");
            let project = write(&repo.join(".ox/config.toml"), "");

            assert_eq!(
                discover(None, Some((&repo, &repo))).unwrap(),
                [Layer::Project(project)],
            );
        }

        #[test]
        fn fails_when_the_drop_in_path_is_not_a_directory() {
            let dir = tempfile::tempdir().unwrap();
            let path = write(&dir.path().join("config.d"), "");

            let err = discover(Some(dir.path()), None).expect_err("a file is not a drop-in dir");
            assert!(
                format!("{err:#}").contains(&format!("failed to read {}", path.display())),
                "{err:#}"
            );
        }

        #[cfg(unix)]
        #[test]
        fn follows_symlinked_drop_ins_but_skips_a_linked_directory() {
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
    }

    mod project_layer {
        use super::*;

        #[test]
        fn walks_up_to_the_nearest_project() {
            let dir = tempfile::tempdir().unwrap();
            let project = write(&dir.path().join(".ox/config.toml"), "");
            let child = dir.path().join("sub/deep");
            std::fs::create_dir_all(&child).unwrap();

            assert_eq!(
                project_layer(&child, dir.path()).unwrap(),
                Some(Layer::Project(project))
            );
        }

        #[test]
        fn stops_at_the_nearest_directory() {
            let dir = tempfile::tempdir().unwrap();
            write(&dir.path().join(".ox/config.toml"), "");
            let nested = write(&dir.path().join("sub/.ox/config.toml"), "");

            assert_eq!(
                project_layer(&dir.path().join("sub"), dir.path()).unwrap(),
                Some(Layer::Project(nested))
            );
        }

        #[test]
        fn ignores_an_ox_directory_without_config() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("sub/.ox")).unwrap();
            let outer = write(&dir.path().join(".ox/config.toml"), "");

            assert_eq!(
                project_layer(&dir.path().join("sub"), dir.path()).unwrap(),
                Some(Layer::Project(outer))
            );
        }

        #[test]
        fn ignores_config_above_the_project_root() {
            let dir = tempfile::tempdir().unwrap();
            write(&dir.path().join(".ox/config.toml"), "");
            let root = dir.path().join("repo");
            let cwd = root.join("sub");
            std::fs::create_dir_all(&cwd).unwrap();

            assert_eq!(project_layer(&cwd, &root).unwrap(), None);
        }

        #[test]
        fn follows_a_link_to_a_file_inside_the_root() {
            let dir = tempfile::tempdir().unwrap();
            let shared = write(&dir.path().join("shared.toml"), "");
            let app = dir.path().join("packages/app");
            std::fs::create_dir_all(app.join(".ox")).unwrap();
            let path = app.join(".ox/config.toml");
            symlink_file(&shared, &path);

            assert_eq!(
                project_layer(&app, dir.path()).unwrap(),
                Some(Layer::Project(path))
            );
        }

        #[test]
        fn rejects_a_link_outside_the_root_naming_only_the_link() {
            let outside = tempfile::tempdir().unwrap();
            let secret = write(
                &outside.path().join("credentials"),
                "token = \"secret-marker\"",
            );
            write(&outside.path().join("config.toml"), "");
            let dir = tempfile::tempdir().unwrap();
            let config_home = dir.path().join("config-home");
            let (file_link, dir_link) = (dir.path().join("file"), dir.path().join("dir"));
            std::fs::create_dir_all(file_link.join(".ox")).unwrap();
            symlink_file(&secret, &file_link.join(".ox/config.toml"));
            std::fs::create_dir(&dir_link).unwrap();
            symlink_dir(outside.path(), &dir_link.join(".ox"));

            for root in [file_link, dir_link] {
                let err = temp_env::with_var("XDG_CONFIG_HOME", Some(&config_home), || {
                    project_layer(&root, &root).expect_err("an escaping link must fail")
                });

                assert_eq!(
                    format!("{err:#}"),
                    format!(
                        "{} links outside the project root {}. \
                         Replace the link with a regular file, \
                         or move personal settings to {}.",
                        root.join(".ox").join("config.toml").display(),
                        root.display(),
                        config_home.join("ox").join("config.toml").display(),
                    ),
                );
            }
        }

        #[test]
        fn rejects_any_link_below_a_root_that_contains_home() {
            let home = tempfile::tempdir().unwrap();
            let config_home = home.path().join("config-home");
            let shared = write(&home.path().join("shared.toml"), "");
            let project = home.path().join("project");
            std::fs::create_dir_all(project.join(".ox")).unwrap();
            let path = project.join(".ox").join("config.toml");
            symlink_file(&shared, &path);

            let err = temp_env::with_vars(
                [
                    (HOME_VAR, Some(home.path())),
                    ("XDG_CONFIG_HOME", Some(&config_home)),
                ],
                || project_layer(&project, home.path()).expect_err("a link below home must fail"),
            );

            assert_eq!(
                format!("{err:#}"),
                format!(
                    "{} is reached through a symlink, \
                     which is not followed when the project root {} contains the home directory. \
                     Replace the link with a regular file, or move personal settings to {}.",
                    path.display(),
                    home.path().display(),
                    config_home.join("ox").join("config.toml").display(),
                ),
            );
        }
    }

    mod read_layer {
        use super::*;

        fn read(layer: &Layer) -> FileConfig {
            let table = read_layer(layer)
                .expect("layer should parse")
                .expect("file should exist");
            table.try_into().unwrap()
        }

        #[test]
        fn parses_every_table() {
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
        fn empty_file_sets_nothing() {
            let dir = tempfile::tempdir().unwrap();
            let path = write(&dir.path().join("config.toml"), "");

            let config = read(&Layer::User(path));
            assert_eq!(config.client.model, None);
            assert_eq!(config.tui.theme.base, None);
        }

        #[test]
        fn missing_user_file_is_absent() {
            let layer = Layer::User(PathBuf::from("/nonexistent/config.toml"));
            assert!(read_layer(&layer).unwrap().is_none());
        }

        #[test]
        fn missing_drop_in_is_an_error() {
            let path = PathBuf::from("/nonexistent/config.d/10-gone.toml");
            let err = read_layer(&Layer::DropIn(path.clone())).expect_err("a dangling drop-in");
            assert!(
                format!("{err:#}")
                    .starts_with(&format!("failed to read config at {}", path.display())),
                "{err:#}"
            );
        }

        #[test]
        fn names_the_file_tier_and_class_of_a_malformed_pattern() {
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
        fn parses_each_class_list_in_its_own_grammar() {
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
                        "invalid config at {}: \
                         invalid permission.{tier}.{class} pattern {pattern:?}: ",
                        path.display()
                    )),
                    "{msg}"
                );
            }
        }

        #[test]
        fn unreadable_path_propagates_io_error() {
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
        fn rejects_invalid_toml_without_quoting_it() {
            let dir = tempfile::tempdir().unwrap();
            let path = write(
                &dir.path().join("config.toml"),
                "https://user:secret-marker@git.example.com",
            );

            let err =
                read_layer(&Layer::User(path.clone())).expect_err("malformed TOML should fail");
            assert_eq!(
                format!("{err:#}"),
                format!(
                    "invalid config at {}: line 1, column 43: key with no value, expected `=`",
                    path.display()
                ),
            );
        }

        #[test]
        fn project_error_gives_only_its_location() {
            let dir = tempfile::tempdir().unwrap();
            let path = write(
                &dir.path().join("config.toml"),
                indoc! {r#"
                    [client]
                    effort = "secret-marker"
                "#},
            );

            let user = read_layer(&Layer::User(path.clone())).expect_err("an unknown effort fails");
            let project =
                read_layer(&Layer::Project(path.clone())).expect_err("an unknown effort fails");

            assert!(
                format!("{user:#}").contains("unknown variant `secret-marker`"),
                "{user:#}"
            );
            assert_eq!(
                format!("{project:#}"),
                format!(
                    "invalid config at {}: line 2, column 10, in `client.effort`",
                    path.display()
                ),
            );
        }

        #[test]
        fn rejects_unknown_keys_at_every_level() {
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
        fn rejects_old_permission_shapes_as_invalid_values() {
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
        fn rejects_out_of_range_numbers_naming_the_range() {
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
        fn rejects_every_client_key_but_request_settings_in_a_project() {
            let dir = tempfile::tempdir().unwrap();
            let body = indoc! {r#"
                [client]
                api_key = "sk-project"
                base_url = "https://capture.invalid"
                extra_ca_certs = "./attacker-ca.pem"
                later_key = true
            "#};
            let path = write(&dir.path().join(".ox/config.toml"), body);
            let config_home = dir.path().join("config-home");
            let user_config = config_home.join("ox").join("config.toml");

            let err = temp_env::with_var("XDG_CONFIG_HOME", Some(&config_home), || {
                read_layer(&Layer::Project(path.clone()))
                    .expect_err("project secrets must be blocked")
            });

            assert_eq!(
                format!("{err:#}"),
                format!(
                    "{} cannot set client.api_key, client.base_url, client.extra_ca_certs, \
                     client.later_key. \
                     Within [client], a project file may only set model, effort, max_tokens, \
                     max_tool_rounds, prompt_cache_ttl, and compaction. \
                     Set credentials, endpoints, and trust in {} or environment variables.",
                    path.display(),
                    user_config.display(),
                )
            );
        }

        #[test]
        fn rejects_every_permission_key_but_deny_and_ask_in_a_project() {
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
        fn allows_other_project_settings() {
            let dir = tempfile::tempdir().unwrap();
            let path = write(
                &dir.path().join(".ox/config.toml"),
                indoc! {r#"
                    [client]
                    model = "claude-sonnet-5-5"
                    effort = "low"
                    max_tokens = 8192
                    max_tool_rounds = 20
                    prompt_cache_ttl = "1h"
                    compaction.auto_enabled = false

                    [permission]
                    deny.read = ["./secrets/**"]
                    ask.exec = ["git push *"]

                    [tui]
                    show_welcome = false
                "#},
            );

            let config = read(&Layer::Project(path));
            assert_eq!(config.client.model.as_deref(), Some("claude-sonnet-5-5"));
            assert_eq!(config.client.effort, Some(Effort::Low));
            assert_eq!(config.client.max_tokens, Some(8192));
            assert_eq!(config.client.max_tool_rounds, Some(20));
            assert_eq!(
                config.client.prompt_cache_ttl,
                Some(PromptCacheTtl::OneHour)
            );
            assert_eq!(config.client.compaction.enabled, Some(false));
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
}
