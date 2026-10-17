//! Layer merging. Tables merge by key, permission lists concatenate, and any other value a higher
//! layer sets replaces the lower one. Records which layer set each key.

use std::collections::HashMap;

use toml::{Table, Value};

use super::provenance::{Layer, Source, Sourced};

/// Tables whose arrays concatenate across layers, lowest layer first, so a higher layer adds rules
/// without discarding the lower layer's.
const APPENDED_TABLES: &[&str] = &["permission.deny", "permission.ask", "permission.allow"];

/// Arrays outside [`APPENDED_TABLES`] that concatenate the same way.
const APPENDED: &[&str] = &["permission.extra_roots"];

/// Keys under one table that replace each other: a layer that sets any of them clears the rest
/// from lower layers, so a project's percent threshold takes precedence over a user's token
/// threshold.
const EXCLUSIVE: &[(&str, &[&str])] = &[(
    "client.compaction",
    &["auto_threshold_tokens", "auto_threshold_percent"],
)];

// ── Origins ──

/// The layers that merged, lowest first, and the layer that last set each dotted key.
#[derive(Debug, Default)]
pub(super) struct Origins {
    layers: Vec<Layer>,
    keys: HashMap<String, usize>,
}

impl Origins {
    pub(super) fn source(&self, key: &str) -> Source {
        self.keys.get(key).map_or(Source::Default, |&index| {
            Source::File(self.layers[index].clone())
        })
    }

    /// Pairs a merged value with the layer that set `key`.
    pub(super) fn sourced<T>(&self, key: &str, value: Option<T>) -> Option<Sourced<T>> {
        debug_assert!(
            value.is_none() || self.keys.contains_key(key),
            "no layer recorded for `{key}`, which a merged value sets"
        );
        value.map(|value| Sourced::new(value, self.source(key)))
    }

    pub(super) fn into_layers(self) -> Vec<Layer> {
        self.layers
    }
}

// ── Merge ──

/// Merges layer tables, lowest precedence first.
pub(super) fn merge(layers: Vec<(Layer, Table)>) -> (Table, Origins) {
    let mut merged = Table::new();
    let mut origins = Origins::default();
    for (index, (layer, table)) in layers.into_iter().enumerate() {
        merge_table(&mut merged, table, "", index, &mut origins.keys);
        origins.layers.push(layer);
    }
    (merged, origins)
}

fn merge_table(
    base: &mut Table,
    overlay: Table,
    prefix: &str,
    layer: usize,
    keys: &mut HashMap<String, usize>,
) {
    clear_exclusive(base, &overlay, prefix, keys);
    for (key, value) in overlay {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match (base.get_mut(&key), value) {
            (Some(Value::Table(base)), Value::Table(overlay)) => {
                merge_table(base, overlay, &path, layer, keys);
            }
            (Some(Value::Array(base)), Value::Array(overlay))
                if APPENDED.contains(&path.as_str()) || APPENDED_TABLES.contains(&prefix) =>
            {
                base.extend(overlay);
                keys.insert(path, layer);
            }
            (_, value) => {
                forget(&path, keys);
                record(&value, path, layer, keys);
                base.insert(key, value);
            }
        }
    }
}

fn clear_exclusive(
    base: &mut Table,
    overlay: &Table,
    prefix: &str,
    keys: &mut HashMap<String, usize>,
) {
    for (table, group) in EXCLUSIVE {
        if *table != prefix || !group.iter().any(|key| overlay.contains_key(*key)) {
            continue;
        }
        for key in *group {
            base.remove(*key);
            keys.remove(&format!("{prefix}.{key}"));
        }
    }
}

/// Drops the attributions at or under `path`, whose value is being replaced.
fn forget(path: &str, keys: &mut HashMap<String, usize>) {
    keys.retain(|key, _| {
        key.strip_prefix(path)
            .is_none_or(|rest| !rest.is_empty() && !rest.starts_with('.'))
    });
}

/// Attributes every leaf under `value` to `layer`.
fn record(value: &Value, path: String, layer: usize, keys: &mut HashMap<String, usize>) {
    if let Value::Table(table) = value {
        for (key, value) in table {
            record(value, format!("{path}.{key}"), layer, keys);
        }
    } else {
        keys.insert(path, layer);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use indoc::indoc;

    use super::*;

    fn layer(name: &str) -> Layer {
        Layer::DropIn(PathBuf::from(format!("/ox/config.d/{name}.toml")))
    }

    fn merged(layers: &[(&str, &str)]) -> (Table, Origins) {
        merge(
            layers
                .iter()
                .map(|(name, body)| (layer(name), body.parse::<Table>().unwrap()))
                .collect(),
        )
    }

    fn get<'a>(table: &'a Table, path: &str) -> Option<&'a Value> {
        let mut parts = path.split('.');
        let first = table.get(parts.next()?)?;
        parts.try_fold(first, |value, key| value.get(key))
    }

    fn strings(table: &Table, path: &str) -> Vec<String> {
        get(table, path)
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect()
    }

    // ── merge ──

    #[test]
    fn merge_tables_by_key_with_the_higher_layer_winning_each_key() {
        let (table, origins) = merged(&[
            (
                "a",
                indoc! {r##"
                    [client]
                    model = "low-model"
                    max_tokens = 100

                    [tui.theme.overrides]
                    error = { fg = "#aaaaaa" }
                "##},
            ),
            (
                "b",
                indoc! {r##"
                    [client]
                    model = "high-model"

                    [tui.theme.overrides]
                    error = { bold = true }
                    accent = "#bbbbbb"
                "##},
            ),
        ]);

        assert_eq!(
            get(&table, "client.model").and_then(Value::as_str),
            Some("high-model")
        );
        assert_eq!(
            get(&table, "client.max_tokens").and_then(Value::as_integer),
            Some(100)
        );
        assert_eq!(
            get(&table, "tui.theme.overrides.error"),
            Some(&Value::Table(
                indoc! {r##"
                    fg = "#aaaaaa"
                    bold = true
                "##}
                .parse()
                .unwrap()
            )),
        );
        assert_eq!(origins.source("client.model"), Source::File(layer("b")));
        assert_eq!(
            origins.source("client.max_tokens"),
            Source::File(layer("a"))
        );
        assert_eq!(
            origins.source("tui.theme.overrides.error.fg"),
            Source::File(layer("a"))
        );
        assert_eq!(
            origins.source("tui.theme.overrides.accent"),
            Source::File(layer("b"))
        );
        assert_eq!(origins.source("client.effort"), Source::Default);
        assert_eq!(origins.into_layers(), [layer("a"), layer("b")]);
    }

    #[test]
    fn merge_concatenates_permission_lists_lowest_layer_first() {
        let (table, origins) = merged(&[
            (
                "a",
                indoc! {r#"
                    [permission]
                    extra_roots = ["/srv/a"]
                    allow.exec = ["ls"]
                    deny.exec = ["rm *"]
                "#},
            ),
            ("b", r#"permission.mode = "plan""#),
            (
                "c",
                indoc! {r#"
                    [permission]
                    extra_roots = ["/srv/b"]
                    allow.exec = ["cat *"]
                    ask.edit = ["./Cargo.toml"]
                    deny.exec = ["curl *", "rm *"]
                "#},
            ),
        ]);

        assert_eq!(
            strings(&table, "permission.extra_roots"),
            ["/srv/a", "/srv/b"]
        );
        assert_eq!(strings(&table, "permission.allow.exec"), ["ls", "cat *"]);
        assert_eq!(strings(&table, "permission.ask.edit"), ["./Cargo.toml"]);
        assert_eq!(
            strings(&table, "permission.deny.exec"),
            ["rm *", "curl *", "rm *"]
        );
        assert_eq!(
            origins.source("permission.deny.exec"),
            Source::File(layer("c"))
        );
    }

    #[test]
    fn merge_replaces_other_arrays() {
        let (table, origins) = merged(&[
            ("a", r#"tui.status_line = ["model", "run-state"]"#),
            ("b", r#"tui.status_line = ["current-dir"]"#),
        ]);

        assert_eq!(strings(&table, "tui.status_line"), ["current-dir"]);
        assert_eq!(origins.source("tui.status_line"), Source::File(layer("b")));
    }

    #[test]
    fn merge_a_value_of_another_shape_replaces_the_lower_one_and_its_origins() {
        let (table, origins) = merged(&[
            ("a", r##"tui.theme.overrides.error = { fg = "#aaaaaa" }"##),
            ("b", r##"tui.theme.overrides.error = "#bbbbbb""##),
            ("c", r##"tui.theme.overrides.accent = "#cccccc""##),
            ("d", "tui.theme.overrides.accent = { bold = true }"),
        ]);

        assert_eq!(
            get(&table, "tui.theme.overrides.error").and_then(Value::as_str),
            Some("#bbbbbb")
        );
        assert_eq!(
            get(&table, "tui.theme.overrides.accent"),
            Some(&Value::Table("bold = true".parse().unwrap())),
        );
        assert_eq!(
            origins.source("tui.theme.overrides.error"),
            Source::File(layer("b"))
        );
        assert_eq!(
            origins.source("tui.theme.overrides.error.fg"),
            Source::Default
        );
        assert_eq!(
            origins.source("tui.theme.overrides.accent"),
            Source::Default
        );
        assert_eq!(
            origins.source("tui.theme.overrides.accent.bold"),
            Source::File(layer("d"))
        );
    }

    #[test]
    fn merge_a_threshold_clears_the_other_threshold_from_lower_layers() {
        let (table, origins) = merged(&[
            (
                "a",
                indoc! {"
                    [client.compaction]
                    auto_enabled = false
                    auto_threshold_tokens = 400000
                "},
            ),
            ("b", "client.compaction.auto_threshold_percent = 40"),
        ]);

        assert_eq!(get(&table, "client.compaction.auto_threshold_tokens"), None);
        assert_eq!(
            get(&table, "client.compaction.auto_threshold_percent").and_then(Value::as_integer),
            Some(40)
        );
        assert_eq!(
            get(&table, "client.compaction.auto_enabled").and_then(Value::as_bool),
            Some(false),
            "only the threshold group is exclusive"
        );
        assert_eq!(
            origins.source("client.compaction.auto_threshold_tokens"),
            Source::Default
        );
    }

    #[test]
    fn merge_a_layer_without_a_threshold_keeps_the_lower_one() {
        let (table, _) = merged(&[
            ("a", "client.compaction.auto_threshold_tokens = 400000"),
            ("b", "client.compaction.auto_enabled = true"),
        ]);

        assert_eq!(
            get(&table, "client.compaction.auto_threshold_tokens").and_then(Value::as_integer),
            Some(400_000)
        );
    }
}
