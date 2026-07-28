use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Default)]
pub struct SystemOptions {
    /// Port origin (e.g. "www/apache24") -> (option_name -> enabled)
    pub port_overrides: HashMap<String, HashMap<String, bool>>,
    /// Global option_name -> enabled
    pub global_overrides: HashMap<String, bool>,
}

impl SystemOptions {
    pub fn load(options_dir: &Path, make_conf: Option<&Path>) -> Self {
        let mut sys_opts = SystemOptions::default();

        // 1. Read global make.conf overrides if provided
        if let Some(m_path) = make_conf {
            if let Ok(content) = fs::read_to_string(m_path) {
                for line in content.lines() {
                    let line = line.trim();
                    if line.starts_with('#') || line.is_empty() {
                        continue;
                    }
                    if let Some(rest) = line.strip_prefix("OPTIONS_SET") {
                        let clean = rest
                            .trim_start_matches(|c| c == '+' || c == ':' || c == '=' || c == ' ');
                        for opt in clean.split_whitespace() {
                            sys_opts.global_overrides.insert(opt.to_string(), true);
                        }
                    } else if let Some(rest) = line.strip_prefix("OPTIONS_UNSET") {
                        let clean = rest
                            .trim_start_matches(|c| c == '+' || c == ':' || c == '=' || c == ' ');
                        for opt in clean.split_whitespace() {
                            sys_opts.global_overrides.insert(opt.to_string(), false);
                        }
                    }
                }
            }
        }

        // 2. Read per-port /var/db/ports/<cat>_<port>/options
        if let Ok(entries) = fs::read_dir(options_dir) {
            for entry in entries.flatten() {
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
                if let Ok(content) = fs::read_to_string(opts_file) {
                    let mut map = HashMap::new();
                    for line in content.lines() {
                        let line = line.trim();
                        if line.starts_with('#') || line.is_empty() {
                            continue;
                        }
                        if let Some(opt) = line
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

        sys_opts
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
