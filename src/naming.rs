//! The app's name in paths and environment variables.
//!
//! - `LUMBRIDGE_NOTES_*` variables ([`env_var`]).
//! - `~/.config/lumbridge-notes/` and `$XDG_STATE_HOME/lumbridge-notes/`
//!   ([`app_file`]).
//! - `<notebook>/.lumbridge/` for templates and prompts (see
//!   [`crate::library`]).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The app's name: binary, config and state folders, runtime folder.
pub const APP: &str = "lumbridge-notes";

const ENV_PREFIX: &str = "LUMBRIDGE_NOTES_";

/// `LUMBRIDGE_NOTES_<name>`. An empty value counts as unset.
pub fn env_var(name: &str) -> Option<OsString> {
    env_var_in(name, |key| std::env::var_os(key))
}

/// [`env_var`] over any variable lookup.
pub(crate) fn env_var_in(
    name: &str,
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Option<OsString> {
    lookup(&format!("{ENV_PREFIX}{name}")).filter(|value| !value.is_empty())
}

/// `<base>/lumbridge-notes/<file>`.
pub fn app_file(base: &Path, file: &str) -> PathBuf {
    base.join(APP).join(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_reads_the_app_prefix_and_an_empty_value_is_unset() {
        let set = |key: &str| (key == "LUMBRIDGE_NOTES_DIR").then(|| OsString::from("/notes"));
        assert_eq!(env_var_in("DIR", set), Some("/notes".into()));
        assert_eq!(env_var_in("THEME", set), None);
        let empty = |key: &str| (key == "LUMBRIDGE_NOTES_DIR").then(OsString::new);
        assert_eq!(env_var_in("DIR", empty), None);
    }

    #[test]
    fn app_files_live_in_the_app_folder() {
        assert_eq!(
            app_file(Path::new("/base"), "voice"),
            PathBuf::from("/base/lumbridge-notes/voice")
        );
    }
}
