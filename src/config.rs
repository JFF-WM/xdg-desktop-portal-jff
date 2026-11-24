//! `$XDG_CONFIG_HOME/jff/portal.conf`: which command picks what to share.
//!
//! ```text
//! # The picker reads one line per source on stdin and prints the chosen line.
//! chooser = ["fuzzel", "--dmenu"]
//! ```
//!
//! `JFF_PORTAL_CHOOSER` (words separated by spaces) overrides the file. No other keys exist: an
//! unknown one is an error, so a typo is not silently ignored.

use std::path::{Path, PathBuf};

/// Picker used when nothing says otherwise.
pub const DEFAULT_CHOOSER: &[&str] = &["fuzzel", "--dmenu"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Program and arguments of the picker.
    pub chooser: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            chooser: DEFAULT_CHOOSER.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// Parse the text of a `portal.conf`.
pub fn parse(text: &str) -> Result<Config, String> {
    let mut config = Config::default();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = |msg: &str| format!("line {}: {msg}", n + 1);
        let Some((key, value)) = line.split_once('=') else {
            return Err(at("expected `key = value`"));
        };
        match key.trim() {
            "chooser" => {
                let words = parse_string_list(value.trim()).map_err(|e| at(&e))?;
                if words.is_empty() || words[0].is_empty() {
                    return Err(at("chooser needs a program"));
                }
                config.chooser = words;
            }
            other => return Err(at(&format!("unknown key `{other}`"))),
        }
    }
    Ok(config)
}

/// `["a", "b c", "d\"e"]` into its strings.
fn parse_string_list(text: &str) -> Result<Vec<String>, String> {
    let inner = text
        .strip_prefix('[')
        .and_then(|t| t.strip_suffix(']'))
        .ok_or("expected a list like [\"program\", \"arg\"]")?;
    let mut out = Vec::new();
    let mut chars = inner.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        match chars.next() {
            None => break,
            Some('"') => {
                let mut word = String::new();
                loop {
                    match chars.next() {
                        None => return Err("unterminated string".into()),
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\')) => word.push(c),
                            _ => return Err("only \\\" and \\\\ can be escaped".into()),
                        },
                        Some(c) => word.push(c),
                    }
                }
                out.push(word);
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
                match chars.next() {
                    None => break,
                    Some(',') => {}
                    Some(c) => return Err(format!("expected `,` after a string, found `{c}`")),
                }
            }
            Some(c) => return Err(format!("expected a string, found `{c}`")),
        }
    }
    Ok(out)
}

/// The picker named by `JFF_PORTAL_CHOOSER`, if it is set and not empty.
pub fn chooser_from_env(value: Option<&str>) -> Option<Vec<String>> {
    let words: Vec<String> = value?.split_whitespace().map(str::to_string).collect();
    (!words.is_empty()).then_some(words)
}

/// Where the config file is.
pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("jff").join("portal.conf"))
}

/// Read the file (a missing one means the defaults) and apply the environment.
pub fn load() -> Result<Config, String> {
    let mut config = match path() {
        Some(p) => load_file(&p)?,
        None => Config::default(),
    };
    if let Some(chooser) = chooser_from_env(std::env::var("JFF_PORTAL_CHOOSER").ok().as_deref()) {
        config.chooser = chooser;
    }
    Ok(config)
}

fn load_file(path: &Path) -> Result<Config, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_the_default() {
        assert_eq!(parse("").unwrap(), Config::default());
        assert_eq!(parse("# nothing\n\n").unwrap(), Config::default());
    }

    #[test]
    fn a_chooser_is_read() {
        let c = parse("chooser = [\"wofi\", \"--dmenu\", \"-p\", \"share what?\"]").unwrap();
        assert_eq!(c.chooser, ["wofi", "--dmenu", "-p", "share what?"]);
    }

    #[test]
    fn escapes_and_spacing() {
        let c = parse("  chooser=[ \"a\\\"b\" ,\"c\\\\d\" ]  ").unwrap();
        assert_eq!(c.chooser, ["a\"b", "c\\d"]);
    }

    #[test]
    fn a_trailing_comma_is_fine() {
        assert_eq!(parse("chooser = [\"x\",]").unwrap().chooser, ["x"]);
    }

    #[test]
    fn bad_input_is_an_error() {
        assert!(parse("chooser").is_err());
        assert!(parse("chooser = fuzzel").is_err());
        assert!(parse("chooser = []").is_err());
        assert!(parse("chooser = [\"\"]").is_err());
        assert!(parse("chooser = [\"a\" \"b\"]").is_err());
        assert!(parse("chooser = [\"a]").is_err());
        assert!(parse("chooser = [\"a\\n\"]").is_err());
        assert!(parse("chooser = [a]").is_err());
        let err = parse("# c\nchoser = [\"a\"]").unwrap_err();
        assert!(err.starts_with("line 2:"), "{err}");
    }

    #[test]
    fn the_environment_splits_on_spaces() {
        assert_eq!(
            chooser_from_env(Some("rofi -dmenu")),
            Some(vec!["rofi".to_string(), "-dmenu".to_string()])
        );
        assert_eq!(chooser_from_env(Some("  ")), None);
        assert_eq!(chooser_from_env(None), None);
    }
}
