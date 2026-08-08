use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

/// Reads `path`, treating a missing file as empty and any other failure as the
/// error it is. Absence is normal — a fresh system has nothing saved — but an
/// unreadable file is not: loading defaults in its place would have the next
/// save overwrite the user's real choices with them.
fn read_or_absent(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
    }
}

/// Strips the assignment operator following a variable name, leaving the
/// whitespace-separated values. Returns `None` when what follows the name is
/// not an assignment, so that `OPTIONS_SET_FORCE=...` is not mistaken for
/// `OPTIONS_SET`.
fn assignment_values(rest: &str) -> Option<&str> {
    let trimmed = rest.trim_start_matches([' ', '\t']);
    if !trimmed.starts_with(['=', '+', ':', '?']) {
        return None;
    }
    Some(trimmed.trim_start_matches(['=', '+', ':', '?', ' ', '\t']))
}

#[derive(Debug, Default, Clone)]
pub struct SystemOptions {
    /// Port origin (e.g. "www/apache24") -> (option_name -> enabled)
    pub port_overrides: HashMap<String, HashMap<String, bool>>,
    /// Global option_name -> enabled
    pub global_overrides: HashMap<String, bool>,
}

impl SystemOptions {
    /// Reads the system's saved option state: `make.conf` overrides, then the
    /// per-port options files.
    ///
    /// A path that does not exist is an empty state. A path that exists but
    /// cannot be read is an error rather than an empty state: the choices are
    /// there and unknown, and a session loaded at defaults in their place
    /// would overwrite them on its first save.
    pub fn load(options_dir: &Path, make_conf: Option<&Path>) -> Result<Self> {
        let mut sys_opts = SystemOptions::default();

        // 1. Read global make.conf overrides if provided
        if let Some(m_path) = make_conf {
            if let Some(content) = read_or_absent(m_path)? {
                for line in content.lines() {
                    let line = line.trim();
                    if line.starts_with('#') || line.is_empty() {
                        continue;
                    }
                    if let Some(clean) =
                        line.strip_prefix("OPTIONS_SET").and_then(assignment_values)
                    {
                        for opt in clean.split_whitespace() {
                            sys_opts.global_overrides.insert(opt.to_string(), true);
                        }
                    } else if let Some(clean) = line
                        .strip_prefix("OPTIONS_UNSET")
                        .and_then(assignment_values)
                    {
                        for opt in clean.split_whitespace() {
                            sys_opts.global_overrides.insert(opt.to_string(), false);
                        }
                    }
                }
            }
        }

        // 2. Read per-port /var/db/ports/<cat>_<port>/options
        let entries = match fs::read_dir(options_dir) {
            Ok(entries) => Some(entries),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "could not read the options directory {}",
                        options_dir.display()
                    )
                })
            }
        };
        if let Some(entries) = entries {
            for entry in entries {
                let entry =
                    entry.with_context(|| format!("could not list {}", options_dir.display()))?;
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let folder_name = match path.file_name().and_then(|s| s.to_str()) {
                    Some(f) => f,
                    None => continue,
                };

                // Split "www_apache24" into "www/apache24"
                let origin = match folder_name.find('_') {
                    Some(idx) => format!("{}/{}", &folder_name[..idx], &folder_name[idx + 1..]),
                    None => continue,
                };

                let opts_file = path.join("options");
                if let Some(content) = read_or_absent(&opts_file)? {
                    let mut map = HashMap::new();
                    for line in content.lines() {
                        let line = line.trim();
                        if line.starts_with('#') || line.is_empty() {
                            continue;
                        }
                        // The format `make config` and poudriere write. Values
                        // are one-per-line in practice, but `OPTIONS_FILE_SET+=
                        // FOO BAR` is equally valid make, so split on whitespace.
                        if let Some(clean) = line
                            .strip_prefix("OPTIONS_FILE_SET")
                            .and_then(assignment_values)
                        {
                            for opt in clean.split_whitespace() {
                                map.insert(opt.to_string(), true);
                            }
                        } else if let Some(clean) = line
                            .strip_prefix("OPTIONS_FILE_UNSET")
                            .and_then(assignment_values)
                        {
                            for opt in clean.split_whitespace() {
                                map.insert(opt.to_string(), false);
                            }
                        // Files written by bgone before it emitted the format
                        // above. The ports framework never honoured these, but
                        // they still record what the user picked.
                        } else if let Some(opt) = line
                            .strip_prefix("WITH_")
                            .and_then(|s| s.strip_suffix("=true"))
                        {
                            map.insert(opt.trim().to_string(), true);
                        } else if let Some(opt) = line
                            .strip_prefix("WITHOUT_")
                            .and_then(|s| s.strip_suffix("=true"))
                        {
                            map.insert(opt.trim().to_string(), false);
                        }
                    }
                    if !map.is_empty() {
                        sys_opts.port_overrides.insert(origin, map);
                    }
                }
            }
        }

        Ok(sys_opts)
    }

    pub fn get_state(&self, origin: &str, opt_name: &str, default_state: bool) -> bool {
        // Per-port override > Global override > Makefile default
        if let Some(opts) = self.port_overrides.get(origin) {
            if let Some(&state) = opts.get(opt_name) {
                return state;
            }
        }
        if let Some(&state) = self.global_overrides.get(opt_name) {
            return state;
        }
        default_state
    }
}
