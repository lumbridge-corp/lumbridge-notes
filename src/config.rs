//! User settings: `~/.config/lumbridge-notes/config.toml` (or
//! `$LUMBRIDGE_NOTES_CONFIG`).
//!
//! Every service is optional. Without an `[ai]` url the AI and naming buttons
//! say how to set one up; the same goes for `[speech]` and `[dictation]`.
//!
//! ```toml
//! [ai]
//! url = "http://host:8001/v1"      # any OpenAI-compatible server
//! model = "brain"
//! auto_name = true                      # name untitled notes by themselves
//! # Merged into every chat request, e.g. to turn reasoning off:
//! extra = { chat_template_kwargs = { enable_thinking = false } }
//!
//! [speech]
//! url = "http://host:8097/v1"      # OpenAI-style /audio/speech
//! model = "voxcpm2"
//! voice = "default"
//!
//! [dictation]
//! url = "http://host:8791/v1"      # OpenAI-style /audio/transcriptions
//! punctuate = true                    # let the AI punctuate what was said
//! live = true                         # words appear while you talk
//! ```

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
    pub ai: AiConfig,
    pub speech: SpeechConfig,
    pub dictation: DictationConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    /// Base url ending in `/v1`; empty turns AI off.
    pub url: String,
    pub model: String,
    /// Name of an environment variable holding a bearer token, if needed.
    pub api_key_env: String,
    /// Name untitled notes once they have some text.
    pub auto_name: bool,
    /// Extra fields merged into each chat request body.
    pub extra: toml::Table,
    /// Seconds before a request is given up.
    pub timeout: u64,
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig {
            url: String::new(),
            model: String::new(),
            api_key_env: String::new(),
            auto_name: true,
            extra: toml::Table::new(),
            timeout: 120,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SpeechConfig {
    pub url: String,
    pub model: String,
    pub voice: String,
    /// Command that plays a WAV file given as its last argument; empty picks
    /// the first of pw-play, paplay, aplay on PATH.
    pub player: Vec<String>,
}

impl Default for SpeechConfig {
    fn default() -> Self {
        SpeechConfig {
            url: String::new(),
            model: String::new(),
            voice: "default".to_owned(),
            player: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct DictationConfig {
    pub url: String,
    pub model: String,
    /// Command that records 16 kHz mono WAV to the file given as its last
    /// argument until it gets SIGINT; empty picks pw-record, parecord or
    /// arecord.
    pub recorder: Vec<String>,
    /// Longest recording, in seconds.
    pub max_seconds: u64,
    /// Have the AI add punctuation and paragraphs to what was dictated
    /// (when `[ai]` is set up).
    pub punctuate: bool,
    /// Stream the microphone so words appear while you talk (needs the
    /// server's `/stream` WebSocket); off records a take and sends it after.
    pub live: bool,
    /// Command that writes raw 16 kHz signed 16-bit mono PCM to stdout until
    /// it gets SIGINT, for live dictation; empty picks pw-record, parecord or
    /// arecord.
    pub live_recorder: Vec<String>,
}

impl Default for DictationConfig {
    fn default() -> Self {
        DictationConfig {
            url: String::new(),
            model: String::new(),
            recorder: Vec::new(),
            max_seconds: 600,
            punctuate: true,
            live: true,
            live_recorder: Vec::new(),
        }
    }
}

impl Config {
    /// The config file's path: `$LUMBRIDGE_NOTES_CONFIG`, else
    /// `lumbridge-notes/config.toml` in the config folder.
    pub fn path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| crate::store::home_dir().map(|home| home.join(".config")));
        Config::path_from(crate::naming::env_var("CONFIG"), base)
    }

    fn path_from(explicit: Option<OsString>, base: Option<PathBuf>) -> Option<PathBuf> {
        if let Some(path) = explicit {
            return Some(PathBuf::from(path));
        }
        Some(crate::naming::app_file(&base?, "config.toml"))
    }

    /// Loads the config; a missing file is the defaults, a broken one is the
    /// defaults plus the reason (shown in the status line).
    pub fn load() -> (Config, Option<String>) {
        match Config::path() {
            Some(path) => Config::load_from(&path),
            None => (Config::default(), None),
        }
    }

    pub fn load_from(path: &Path) -> (Config, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(config) => (config, None),
                Err(err) => (
                    Config::default(),
                    Some(format!("{}: {}", path.display(), err.message())),
                ),
            },
            Err(_) => (Config::default(), None),
        }
    }

    pub fn ai_ready(&self) -> bool {
        !self.ai.url.trim().is_empty()
    }

    pub fn speech_ready(&self) -> bool {
        !self.speech.url.trim().is_empty()
    }

    pub fn dictation_ready(&self) -> bool {
        !self.dictation.url.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_services_and_keeps_defaults() {
        let config: Config = toml::from_str(
            r#"
[ai]
url = "http://h:8001/v1"
model = "brain"
extra = { chat_template_kwargs = { enable_thinking = false } }

[speech]
url = "http://h:8097/v1"
voice = "sam"
"#,
        )
        .unwrap();
        assert!(config.ai_ready() && config.speech_ready());
        assert!(!config.dictation_ready());
        assert!(config.ai.auto_name);
        assert_eq!(config.speech.voice, "sam");
        assert!(config.ai.extra.contains_key("chat_template_kwargs"));
        assert_eq!(config.dictation.max_seconds, 600);
        assert!(!Config::default().ai_ready());
    }

    #[test]
    fn config_path_prefers_the_variable_then_the_config_folder() {
        let dir = crate::test_support::TempDir::new("config-path");
        let base = dir.path().to_path_buf();
        assert_eq!(
            Config::path_from(Some("/etc/notes.toml".into()), Some(base.clone())),
            Some(PathBuf::from("/etc/notes.toml"))
        );
        assert_eq!(Config::path_from(None, None), None);
        let new = base.join("lumbridge-notes").join("config.toml");
        assert_eq!(Config::path_from(None, Some(base)), Some(new));
    }

    #[test]
    fn a_broken_file_falls_back_with_a_reason() {
        let dir = crate::test_support::TempDir::new("config");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[ai]\nurl = 3\n").unwrap();
        let (config, err) = Config::load_from(&path);
        assert_eq!(config, Config::default());
        assert!(err.is_some());
        let (_, none) = Config::load_from(&dir.path().join("missing.toml"));
        assert!(none.is_none());
    }
}
