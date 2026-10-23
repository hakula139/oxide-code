//! Layer merging. Tables merge by key, permission lists concatenate, and any other value a higher
//! layer sets replaces the lower one. Records the layer behind each key and appended element.

use std::collections::HashMap;
use std::iter;

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

/// The layers that merged, lowest first, the layer that last set each dotted key, and the layer
/// that added each element of a concatenated array.
#[derive(Debug, Default)]
pub(super) struct Origins {
    layers: Vec<Layer>,
    keys: HashMap<String, usize>,
    elements: HashMap<String, Vec<usize>>,
}

impl Origins {
    pub(super) fn source(&self, key: &str) -> Source {
        self.keys
            .get(key)
            .map_or(Source::Default, |&index| self.layer_source(index))
    }

    /// Pairs each element of the concatenated array at `key` with the layer that added it.
    pub(super) fn sourced_elements<T>(
        &self,
        key: &str,
        values: impl IntoIterator<Item = T>,
    ) -> Vec<Sourced<T>> {
        let layers = self.elements.get(key).map_or(&[][..], Vec::as_slice);
        let values: Vec<T> = values.into_iter().collect();
        debug_assert_eq!(
            values.len(),
            layers.len(),
            "`{key}` has a different element count than its recorded layers"
        );
        values
            .into_iter()
            .zip(layers)
            .map(|(value, &index)| Sourced::new(value, self.layer_source(index)))
            .collect()
    }

    /// The layers that set any key under the table at `prefix`, lowest first.
    pub(super) fn sources_under(&self, prefix: &str) -> Vec<Source> {
        let mut layers: Vec<usize> = self
            .keys
            .iter()
            .filter(|(key, _)| is_under(key, prefix))
            .map(|(_, &index)| index)
            .collect();
        layers.sort_unstable();
        layers.dedup();
        layers
            .into_iter()
            .map(|index| self.layer_source(index))
            .collect()
    }

    fn layer_source(&self, index: usize) -> Source {
        Source::File(self.layers[index].clone())
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

    /// Drops the attributions at or under `path`, whose value is being replaced.
    fn forget(&mut self, path: &str) {
        let replaced = |key: &String| key == path || is_under(key, path);
        self.keys.retain(|key, _| !replaced(key));
        self.elements.retain(|key, _| !replaced(key));
    }

    /// Attributes every leaf under `value` to `layer`.
    fn record(&mut self, value: &Value, path: String, layer: usize) {
        match value {
            Value::Table(table) => {
                for (key, value) in table {
                    self.record(value, format!("{path}.{key}"), layer);
                }
            }
            Value::Array(items) if is_appended(&path) => {
                self.elements.insert(path.clone(), vec![layer; items.len()]);
                self.keys.insert(path, layer);
            }
            _ => {
                self.keys.insert(path, layer);
            }
        }
    }
}

fn is_under(key: &str, prefix: &str) -> bool {
    key.strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('.'))
}

fn is_appended(path: &str) -> bool {
    APPENDED.contains(&path)
        || path
            .rsplit_once('.')
            .is_some_and(|(table, _)| APPENDED_TABLES.contains(&table))
}

// ── Merge ──

/// Merges layer tables, lowest precedence first.
pub(super) fn merge(layers: Vec<(Layer, Table)>) -> (Table, Origins) {
    let mut merged = Table::new();
    let mut origins = Origins::default();
    for (index, (layer, table)) in layers.into_iter().enumerate() {
        merge_table(&mut merged, table, "", index, &mut origins);
        origins.layers.push(layer);
    }
    (merged, origins)
}

fn merge_table(
    base: &mut Table,
    overlay: Table,
    prefix: &str,
    layer: usize,
    origins: &mut Origins,
) {
    clear_exclusive(base, &overlay, prefix, origins);
    for (key, value) in overlay {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match (base.get_mut(&key), value) {
            (Some(Value::Table(base)), Value::Table(overlay)) => {
                merge_table(base, overlay, &path, layer, origins);
            }
            (Some(Value::Array(base)), Value::Array(overlay)) if is_appended(&path) => {
                origins
                    .elements
                    .entry(path.clone())
                    .or_default()
                    .extend(iter::repeat_n(layer, overlay.len()));
                origins.keys.insert(path, layer);
                base.extend(overlay);
            }
            (_, value) => {
                origins.forget(&path);
                origins.record(&value, path, layer);
                base.insert(key, value);
            }
        }
    }
}

fn clear_exclusive(base: &mut Table, overlay: &Table, prefix: &str, origins: &mut Origins) {
    for (table, group) in EXCLUSIVE {
        if *table != prefix || !group.iter().any(|key| overlay.contains_key(*key)) {
            continue;
        }
        for key in *group {
            base.remove(*key);
            origins.keys.remove(&format!("{prefix}.{key}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use indoc::indoc;

    use super::*;

    mod merge {
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

        #[test]
        fn tables_by_key_with_the_higher_layer_winning_each_key() {
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
            assert_eq!(
                origins.sources_under("tui.theme.overrides"),
                [Source::File(layer("a")), Source::File(layer("b"))]
            );
            assert_eq!(origins.sources_under("client.compaction"), []);
            assert_eq!(origins.into_layers(), [layer("a"), layer("b")]);
        }

        #[test]
        fn concatenates_permission_lists_lowest_layer_first() {
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
            let sourced = |key: &str| origins.sourced_elements(key, strings(&table, key));
            let from =
                |value: &str, name: &str| Sourced::new(value.to_owned(), Source::File(layer(name)));
            assert_eq!(
                sourced("permission.extra_roots"),
                [from("/srv/a", "a"), from("/srv/b", "c")]
            );
            assert_eq!(
                sourced("permission.deny.exec"),
                [from("rm *", "a"), from("curl *", "c"), from("rm *", "c")]
            );
            assert_eq!(sourced("permission.ask.edit"), [from("./Cargo.toml", "c")]);
        }

        #[test]
        fn replaces_other_arrays() {
            let (table, origins) = merged(&[
                ("a", r#"tui.status_line = ["model", "run-state"]"#),
                ("b", r#"tui.status_line = ["current-dir"]"#),
            ]);

            assert_eq!(strings(&table, "tui.status_line"), ["current-dir"]);
            assert_eq!(origins.source("tui.status_line"), Source::File(layer("b")));
        }

        #[test]
        fn a_value_of_another_shape_replaces_the_lower_one_and_its_origins() {
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
        fn a_threshold_clears_the_other_threshold_from_lower_layers() {
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
        fn a_layer_without_a_threshold_keeps_the_lower_one() {
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
}
