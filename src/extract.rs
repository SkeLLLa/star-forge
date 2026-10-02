//! Value extraction (trim/regex/json) and `{value}` template rendering.

use regex::Regex;
use serde::Deserialize;

/// Extraction strategy applied to raw provider output, compiled once at config load.
#[derive(Debug, Clone)]
pub enum Extract {
    Trim,
    Regex { re: Regex, group: usize },
    Json { pointer: String },
}

/// Deserialize-only mirror of [`Extract`]; the regex is compiled afterward.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RawExtract {
    Trim {},
    Regex { pattern: String, group: usize },
    Json { pointer: String },
}

impl RawExtract {
    /// Compiles regex patterns, turning this into a ready-to-use [`Extract`].
    pub fn compile(self) -> Result<Extract, String> {
        Ok(match self {
            Self::Trim {} => Extract::Trim,
            Self::Regex { pattern, group } => {
                let re =
                    Regex::new(&pattern).map_err(|e| format!("invalid regex {pattern:?}: {e}"))?;
                let captures_len = re.captures_len();
                if group >= captures_len {
                    return Err(format!(
                        "regex group {group} is out of range for pattern {pattern:?}; \
                         valid group indices are 0..{captures_len}"
                    ));
                }
                Extract::Regex { re, group }
            }
            Self::Json { pointer } => {
                validate_json_pointer(&pointer)?;
                Extract::Json { pointer }
            }
        })
    }
}

/// Validates the RFC 6901 JSON Pointer string form accepted by `serde_json::Value::pointer`.
fn validate_json_pointer(pointer: &str) -> Result<(), String> {
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return Err(format!(
            "invalid JSON pointer {pointer:?}: a non-empty pointer must start with `/`"
        ));
    }

    let bytes = pointer.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'~' {
            match bytes.get(index + 1) {
                Some(escape) if *escape == b'0' || *escape == b'1' => index += 2,
                _ => {
                    return Err(format!(
                        "invalid JSON pointer {pointer:?}: `~` must be escaped as `~0` or `~1`"
                    ));
                }
            }
        } else {
            index += 1;
        }
    }

    Ok(())
}

/// Applies extraction to raw bytes.
pub fn apply(extract: Option<&Extract>, raw: &[u8]) -> Result<String, String> {
    match extract {
        None | Some(Extract::Trim) => Ok(String::from_utf8_lossy(raw).trim().to_string()),
        Some(Extract::Regex { re, group }) => {
            let text = String::from_utf8_lossy(raw);
            let caps = re.captures(&text).ok_or("regex did not match")?;
            caps.get(*group)
                .map(|m| m.as_str().to_string())
                .ok_or_else(|| format!("regex has no group {group}"))
        }
        Some(Extract::Json { pointer }) => {
            let value: serde_json::Value =
                serde_json::from_slice(raw).map_err(|e| e.to_string())?;
            let found = value.pointer(pointer).ok_or("json pointer not found")?;
            Ok(match found {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
        }
    }
}

/// Renders the `{value}` template. Only one placeholder is supported. An empty `value`
/// renders as an empty string regardless of `format` — e.g. `format = "✗{value}"` shows
/// nothing when `git_status` reports a clean tree (empty `value`), and empty output ⇒
/// starship hides the module.
// "{value}" is a literal template placeholder, not a forgotten `format!`.
#[allow(clippy::literal_string_with_formatting_args)]
pub fn render(format: &str, value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    format.replace("{value}", value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_strips_whitespace() {
        assert_eq!(apply(Some(&Extract::Trim), b"  hi \n").unwrap(), "hi");
        assert_eq!(apply(None, b"  hi \n").unwrap(), "hi");
    }

    #[test]
    fn regex_extracts_group() {
        let re = RawExtract::Regex {
            pattern: r"^(\d+\.\d+)".into(),
            group: 1,
        }
        .compile()
        .unwrap();
        assert_eq!(apply(Some(&re), b"6.9.1-stable").unwrap(), "6.9");
    }

    #[test]
    fn regex_no_match_errors() {
        let re = RawExtract::Regex {
            pattern: r"^(\d+\.\d+)".into(),
            group: 1,
        }
        .compile()
        .unwrap();
        assert!(apply(Some(&re), b"nope").is_err());
    }

    #[test]
    fn raw_extract_rejects_unknown_fields() {
        assert!(serde_json::from_str::<RawExtract>(r#"{"kind":"trim"}"#).is_ok());

        let invalid = [
            r#"{"kind":"trim","unexpected":true}"#,
            r#"{"kind":"regex","pattern":"x","group":0,"unexpected":true}"#,
            r#"{"kind":"json","pointer":"/x","unexpected":true}"#,
        ];
        for raw in invalid {
            let err = serde_json::from_str::<RawExtract>(raw).unwrap_err();
            assert!(err.to_string().contains("unknown field"), "{raw}: {err}");
        }
    }

    #[test]
    fn regex_group_must_exist() {
        for (pattern, group) in [("no captures", 1), ("(one capture)", 2)] {
            let err = RawExtract::Regex {
                pattern: pattern.into(),
                group,
            }
            .compile()
            .unwrap_err();
            assert!(
                err.contains("out of range"),
                "pattern {pattern:?}, group {group}: {err}"
            );
        }

        assert!(
            RawExtract::Regex {
                pattern: "(one capture)".into(),
                group: 1,
            }
            .compile()
            .is_ok()
        );
    }

    #[test]
    fn json_pointer_extracts_string() {
        let ex = RawExtract::Json {
            pointer: "/ip".into(),
        }
        .compile()
        .unwrap();
        assert_eq!(apply(Some(&ex), br#"{"ip":"1.2.3.4"}"#).unwrap(), "1.2.3.4");
    }

    #[test]
    fn json_pointer_missing_errors() {
        let ex = RawExtract::Json {
            pointer: "/missing".into(),
        }
        .compile()
        .unwrap();
        assert!(apply(Some(&ex), br#"{"ip":"1.2.3.4"}"#).is_err());
    }

    #[test]
    fn json_pointer_syntax_is_validated() {
        for pointer in ["", "/", "/a~0b", "/a~1b", "/a~01b"] {
            assert!(
                RawExtract::Json {
                    pointer: pointer.into(),
                }
                .compile()
                .is_ok(),
                "{pointer:?} should be a valid JSON pointer"
            );
        }

        for pointer in ["ip", "a/b", "/bad~", "/bad~2", "/bad~x"] {
            let err = RawExtract::Json {
                pointer: pointer.into(),
            }
            .compile()
            .unwrap_err();
            assert!(err.contains("invalid JSON pointer"), "{pointer:?}: {err}");
        }
    }

    #[test]
    fn invalid_regex_fails_to_compile() {
        let err = RawExtract::Regex {
            pattern: "(".into(),
            group: 0,
        }
        .compile();
        assert!(err.is_err());
    }

    #[test]
    fn render_template_empty_value_is_empty_output() {
        assert_eq!(render(" {value}", ""), "");
        assert_eq!(render("\u{2717}{value}", ""), "");
        assert_eq!(render("{value}%", "42"), "42%");
    }
}
