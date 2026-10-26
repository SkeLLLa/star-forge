//! Text the daemon produces: group expansion and joining for a `get` answer, raw in-process
//! git values through a badge's format, and the `status` table.

use std::collections::BTreeMap;
use std::time::Instant;

use super::{Daemon, cache};
use crate::config::{BadgeConfig, Config};
use crate::extract;

/// Groups resolve daemon-side: members are fetched exactly as if requested individually,
/// then joined into the group's single line by `response_lines`.
pub(super) fn fetch_names(cfg: &Config, requested: &[String]) -> Vec<String> {
    requested
        .iter()
        .flat_map(|n| {
            cfg.groups
                .get(n)
                .map_or_else(|| std::slice::from_ref(n), |g| &g.badges[..])
        })
        .cloned()
        .collect()
}

/// One output line per requested name: a badge's own value (empty if unknown), or for a
/// group its members' non-empty values joined by the group separator, in member order.
pub(super) fn response_lines(
    cfg: &Config,
    requested: &[String],
    values: &BTreeMap<String, String>,
) -> Vec<String> {
    let get = |n: &String| values.get(n).map_or("", String::as_str);
    requested
        .iter()
        .map(|name| {
            cfg.groups.get(name).map_or_else(
                || get(name).to_string(),
                |g| {
                    g.template.as_ref().map_or_else(
                        || {
                            let parts: Vec<_> = g
                                .badges
                                .iter()
                                .map(get)
                                .filter(|v| !v.is_empty())
                                .map(|v| g.output.escape(v))
                                .collect();
                            parts.join(&*g.output.escape(&g.separator))
                        },
                        |t| t.render(g.output, |n| get(&n.to_string()).to_string()),
                    )
                },
            )
        })
        .collect()
}

/// Renders a raw builtin value through the badge's normal extract/format pipeline, without
/// going through `provider::fetch`'s cache/subprocess machinery. Used for the in-process git
/// builtins (see `provider::run_in_process_git`); an extract failure renders empty, same as
/// an empty value would.
pub(super) fn render_raw(badge_cfg: &BadgeConfig, raw: &[u8]) -> String {
    extract::apply(badge_cfg.extract.as_ref(), raw).map_or_else(
        |_| String::new(),
        |value| extract::render(&badge_cfg.format, &value),
    )
}

pub(super) fn build_status(state: &Daemon) -> BTreeMap<String, String> {
    let now = Instant::now();
    let last_error = state
        .config_meta
        .lock()
        .expect("config_meta lock poisoned")
        .last_error
        .clone();
    let mut rows: Vec<(cache::Key, Option<Instant>, u32)> = state
        .core()
        .cache
        .iter()
        .map(|(k, e)| (k.clone(), e.fetched_at, e.errors))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let mut lines = vec![format!(
        "{:<20} {:<40} {:>8} {:>7}",
        "badge", "scope", "age_s", "errors"
    )];
    for (key, fetched_at, errors) in rows {
        let scope = key
            .root
            .as_ref()
            .map_or_else(|| "-".to_string(), |p| p.display().to_string());
        let age = fetched_at.map_or_else(
            || "-".to_string(),
            |t| now.saturating_duration_since(t).as_secs().to_string(),
        );
        lines.push(format!(
            "{:<20} {:<40} {:>8} {:>7}",
            key.badge, scope, age, errors
        ));
    }

    if let Some(e) = last_error {
        lines.push(format!("config error: {e}"));
    }
    if let Some(client) = state.upgrade.last_mismatch() {
        lines.push(format!(
            "version mismatch: client {client}, daemon {}",
            env!("CARGO_PKG_VERSION")
        ));
    }
    let mut values = BTreeMap::new();
    values.insert("status".to_string(), lines.join("\n"));
    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::tests::{test_cfg, test_daemon};
    use crate::template::Dialect;
    use std::time::Duration;

    #[test]
    fn group_lines_skip_empty_keep_order() {
        let mut cfg = test_cfg(Duration::from_secs(60), Duration::from_secs(60));
        cfg.groups.insert(
            "g".into(),
            crate::config::Group {
                badges: vec!["c".into(), "b".into(), "a".into()],
                separator: "|".into(),
                template: None,
                output: Dialect::Ansi,
            },
        );
        let v = |k: &str, x: &str| (k.to_string(), x.to_string());
        let values = BTreeMap::from([v("a", "1"), v("b", ""), v("c", "3")]);
        let req = ["g", "a", "nope"].map(String::from);
        assert_eq!(response_lines(&cfg, &req, &values), ["3|1", "1", ""]);
    }

    #[test]
    fn tmux_plain_group_escapes_values_and_separator() {
        let mut cfg = test_cfg(Duration::from_secs(60), Duration::from_secs(60));
        cfg.groups.insert(
            "g".into(),
            crate::config::Group {
                badges: vec!["a".into(), "b".into()],
                separator: " # ".into(),
                template: None,
                output: Dialect::Tmux,
            },
        );
        let v = |k: &str, x: &str| (k.to_string(), x.to_string());
        let values = BTreeMap::from([v("a", "a#b"), v("b", "#{x}")]);
        let req = ["g"].map(String::from);
        assert_eq!(response_lines(&cfg, &req, &values), ["a##b ## ##{x}"]);
    }

    #[test]
    fn fetch_names_expands_groups_in_order() {
        let mut cfg = test_cfg(Duration::from_secs(60), Duration::from_secs(60));
        cfg.groups.insert(
            "g".into(),
            crate::config::Group {
                badges: vec!["a".into(), "b".into()],
                separator: " ".into(),
                template: None,
                output: Dialect::Ansi,
            },
        );
        let req = ["x", "g", "a", "nope"].map(String::from);
        assert_eq!(fetch_names(&cfg, &req), ["x", "a", "b", "a", "nope"]);
    }

    #[test]
    fn response_lines_template_group_with_plain_and_unknown() {
        let mut cfg = test_cfg(Duration::from_secs(60), Duration::from_secs(60));
        let t = crate::template::Template::parse("[{a}](red)(-{b})", &BTreeMap::new()).unwrap();
        cfg.groups.insert(
            "g".into(),
            crate::config::Group {
                badges: t.vars().to_vec(),
                separator: String::new(),
                template: Some(t),
                output: Dialect::Tmux,
            },
        );
        let v = |k: &str, x: &str| (k.to_string(), x.to_string());
        let values = BTreeMap::from([v("a", "1#"), v("b", "")]);
        let req = ["g", "a", "a", "g"].map(String::from);
        assert_eq!(
            response_lines(&cfg, &req, &values),
            [
                "#[fg=red]1###[default]",
                "1#",
                "1#",
                "#[fg=red]1###[default]"
            ]
        );
    }

    #[test]
    fn status_shows_last_config_error() {
        let state = test_daemon(cache::Power::Ac);
        assert!(!build_status(&state)["status"].contains("config error"));
        state.config_meta.lock().unwrap().last_error = Some("boom".into());
        assert!(build_status(&state)["status"].ends_with("config error: boom"));
    }
}
