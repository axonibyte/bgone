//! Optional TOML configuration.
//!
//! A config is only ever read when `--config` names one; there is no implicit
//! lookup and no default file, so a run without the flag behaves exactly as it
//! did before this existed.
//!
//! Every setting is optional. The file is a *delta* over the built-in defaults,
//! not a complete description of a run: a key that is absent is simply
//! unspecified, and whether that is a problem is decided by the argument itself,
//! never by the config. A key that is *present but misspelled* is a different
//! matter and is rejected, because a setting that silently does nothing is far
//! harder to notice than a failure at startup.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Named sets of ports whose option choices are kept in step, keyed by group
/// name. Ordered so a rewritten file keeps a stable, readable order.
pub type Groups = BTreeMap<String, Vec<String>>;

/// Everything a config file may set.
///
/// Each field mirrors the long form of a command-line argument, so `--options-dir`
/// is `options_dir`. `Option<T>` throughout is what makes the delta rule
/// structural rather than a convention someone has to remember.
#[derive(Debug, Default, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub db_path: Option<PathBuf>,
    pub options_dir: Option<PathBuf>,
    pub make_conf: Option<PathBuf>,
    pub file: Option<PathBuf>,
    pub origins: Option<Vec<String>>,
    pub dry_run: Option<bool>,
    pub force_reset: Option<bool>,
    pub ignore_missing: Option<bool>,
    /// Belongs to the `index` subcommand rather than the top level.
    pub ports_dir: Option<PathBuf>,

    /// Resolution target. These had no config representation until the
    /// poudriere layer needed a tier to sit above: a value derived from a
    /// poudriere spec in the config has to lose to one written here explicitly,
    /// and it cannot lose to something that does not exist.
    pub jail_arch: Option<String>,
    pub osversion: Option<String>,
    pub opsys: Option<String>,
    pub osrel: Option<String>,

    /// Name a poudriere jail, ports tree and set instead of the four settings
    /// above and the two paths. Anything written explicitly wins over what
    /// these derive.
    pub poudriere_etc: Option<PathBuf>,
    pub poudriere_jail: Option<String>,
    pub poudriere_ports: Option<String>,
    pub poudriere_set: Option<String>,
    /// Mirrors `poudriere options -o`: name the options directory outright
    /// rather than composing it from jail/tree/set.
    pub poudriere_optionsdir: Option<String>,
    #[serde(default)]
    pub groups: Groups,
}

impl Config {
    /// Reads a config file.
    ///
    /// A path that does not exist is an error rather than an empty config: it
    /// was named explicitly, so failing to find it is a mistake worth reporting
    /// rather than a silent fallback to defaults.
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("Could not read config file {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("Invalid config file {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }
}

/// Writes `groups` into the config at `path`, leaving the rest of the file as
/// it was.
///
/// Edits the document rather than re-serialising a [`Config`], because a config
/// is something a person writes and comes back to. Round-tripping through the
/// typed struct would silently discard their comments, their key order and any
/// blank lines they used to organise it — a poor trade for saving a group.
///
/// A file that does not exist yet is created holding just the groups. Passing no
/// groups removes the table rather than leaving an empty one behind.
pub fn save_groups(path: &Path, groups: &Groups) -> Result<()> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .with_context(|| format!("Config file {} is not valid TOML", path.display()))?;

    if groups.is_empty() {
        doc.remove("groups");
    } else {
        let mut table = toml_edit::Table::new();
        for (name, members) in groups {
            let mut list = toml_edit::Array::new();
            for member in members {
                list.push(member.as_str());
            }
            table.insert(name, toml_edit::value(list));
        }
        // Written out as a real `[groups]` header rather than inferred from its
        // children, which is what a hand-written file would look like.
        table.set_implicit(false);
        doc.insert("groups", toml_edit::Item::Table(table));
    }

    fs::write(path, doc.to_string())
        .with_context(|| format!("Could not write config file {}", path.display()))
}
