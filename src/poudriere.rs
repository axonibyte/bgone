//! Reads poudriere's own configuration, so you name a jail instead of
//! transcribing what is in it.
//!
//! Everything `bgone index` needs to resolve a ports tree *as a jail* — the
//! architecture, the OS release, the `__FreeBSD_version` — already exists in
//! poudriere's configuration. Typing it again is busywork that fails quietly: a
//! cache resolved for the wrong target is wrong in a way nothing downstream can
//! detect.
//!
//! Nothing here invokes `poudriere`. Its state is a file-per-property attribute
//! store (`attr_set`/`attr_get`, `common.sh:1676`):
//!
//! ```text
//! ${POUDRIERED}/jails/<name>/<property>     arch, version, mnt, method, ...
//! ${POUDRIERED}/ports/<name>/<property>     mnt, method, timestamp
//! ```
//!
//! so reading a property is `read_to_string` and a trim. That is both simpler
//! and more durable than parsing `poudriere jail -i` output, whose format is not
//! a stable interface.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// What a jail says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JailFacts {
    /// The *target* architecture, e.g. `amd64`.
    pub arch: String,
    /// `OSREL` as `bsd.port.mk` means it: `14.4`, not `14.4-RELEASE`.
    pub osrel: String,
    /// `__FreeBSD_version`, read out of the jail's own headers.
    pub osversion: String,
    /// Where the jail is mounted.
    pub mnt: PathBuf,
}

/// A poudriere installation, identified by its etc directory.
#[derive(Debug, Clone)]
pub struct Poudriere {
    poudriered: PathBuf,
}

impl Poudriere {
    /// `etc` is poudriere's configuration root — `/usr/local/etc` unless
    /// `poudriere -e` says otherwise. `POUDRIERED` is `poudriere.d` inside it
    /// (`common.sh:10810`).
    pub fn new(etc: &Path) -> Self {
        Self {
            poudriered: etc.join("poudriere.d"),
        }
    }

    /// Whether this host has poudriere configured at all.
    ///
    /// Absence is not an error — bgone configures ports on machines that have
    /// never heard of poudriere. It only becomes one when a `--poudriere-*`
    /// argument asks for something that cannot be there.
    pub fn is_available(&self) -> bool {
        self.poudriered.is_dir()
    }

    pub fn root(&self) -> &Path {
        &self.poudriered
    }

    /// One property of one jail or ports tree — the whole of `attr_get`.
    ///
    /// An unreadable or empty file is `None` rather than an error: poudriere
    /// treats a missing property as unset, and several are genuinely optional.
    fn attr(&self, kind: &str, name: &str, property: &str) -> Option<String> {
        let value =
            fs::read_to_string(self.poudriered.join(kind).join(name).join(property)).ok()?;
        let value = value.trim().to_string();
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    }

    /// Names of the configured jails, in sorted order.
    ///
    /// Used to say what *is* there when someone asks for one that is not —
    /// `jail_exists` is only a directory test (`common.sh:2381`), so listing is
    /// the same walk.
    pub fn jails(&self) -> Vec<String> {
        self.names_under("jails")
    }

    pub fn trees(&self) -> Vec<String> {
        self.names_under("ports")
    }

    fn names_under(&self, kind: &str) -> Vec<String> {
        let mut names: Vec<String> = match fs::read_dir(self.poudriered.join(kind)) {
            Ok(entries) => entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect(),
            Err(_) => Vec::new(),
        };
        names.sort();
        names
    }

    /// Everything a jail can tell us about how to resolve ports for it.
    pub fn jail(&self, name: &str) -> Result<JailFacts> {
        if !self.poudriered.join("jails").join(name).is_dir() {
            bail!(
                "poudriere has no jail named '{name}'{}",
                self.suggest(&self.jails())
            );
        }

        // poudriere stores `host_arch.target_arch`, and collapses the pair when
        // both halves match (`common.sh:3525`). What a port is built *for* is
        // the target, i.e. everything after the dot (`common.sh:3558`).
        let raw_arch = self
            .attr("jails", name, "arch")
            .with_context(|| format!("jail '{name}' has no arch recorded"))?;
        let arch = raw_arch.rsplit('.').next().unwrap_or(&raw_arch).to_string();

        // `version` is the release string. `OSREL` is that with everything from
        // the first `-` removed — `bsd.port.mk:1152`, `${_OSRELEASE:C/-.*//}`.
        let version = self
            .attr("jails", name, "version")
            .with_context(|| format!("jail '{name}' has no version recorded"))?;
        let osrel = version
            .split('-')
            .next()
            .unwrap_or(&version)
            .trim()
            .to_string();

        let mnt = self
            .attr("jails", name, "mnt")
            .map(PathBuf::from)
            .with_context(|| format!("jail '{name}' has no mount point recorded"))?;

        let osversion = read_freebsd_version(&mnt)?;

        Ok(JailFacts {
            arch,
            osrel,
            osversion,
            mnt,
        })
    }

    /// Where a named ports tree lives — its `mnt` property (`common.sh:3062`).
    pub fn ports_dir(&self, tree: &str) -> Result<PathBuf> {
        if !self.poudriered.join("ports").join(tree).is_dir() {
            bail!(
                "poudriere has no ports tree named '{tree}'{}",
                self.suggest(&self.trees())
            );
        }
        self.attr("ports", tree, "mnt")
            .map(PathBuf::from)
            .with_context(|| format!("ports tree '{tree}' has no mount point recorded"))
    }

    /// The options directory `poudriere options` would write to, given the same
    /// jail, tree and set.
    ///
    /// Reproduced from `options.sh:177` rather than chosen:
    ///
    /// ```text
    /// ${JAILNAME}${JAILNAME:+-}${PTNAME_TMP}${PTNAME_TMP:+-}${SETNAME}${SETNAME:+-}options
    /// ```
    ///
    /// — the non-empty of the three joined with `-`, then `-options`; all three
    /// absent gives a bare `options`. So naming only the tree yields the
    /// tree-keyed directory that survives replacing a jail, and naming the jail
    /// as well yields exactly what `poudriere options -j J -p P` writes.
    pub fn options_dir(
        &self,
        jail: Option<&str>,
        tree: Option<&str>,
        set: Option<&str>,
    ) -> PathBuf {
        let parts: Vec<&str> = [jail, tree, set]
            .into_iter()
            .flatten()
            .filter(|p| !p.is_empty())
            .collect();

        let name = if parts.is_empty() {
            "options".to_string()
        } else {
            format!("{}-options", parts.join("-"))
        };
        self.poudriered.join(name)
    }

    /// The `-o` form: a directory named outright rather than composed.
    pub fn named_options_dir(&self, name: &str) -> PathBuf {
        self.poudriered.join(format!("{name}-options"))
    }

    fn suggest(&self, known: &[String]) -> String {
        if known.is_empty() {
            format!(" (nothing configured under {})", self.poudriered.display())
        } else {
            format!("; configured: {}", known.join(", "))
        }
    }
}

/// Pulls `__FreeBSD_version` out of a jail's headers.
///
/// This is not a stored property — poudriere reads it the same way, by awking
/// the jail's own `param.h` (`common.sh:3854`), which is also where
/// `bsd.port.mk:1158` gets it. Deriving it from the release instead is possible
/// arithmetic (`bsd.port.mk:1137` does the inverse) but it would be a guess, and
/// a wrong `OSVERSION` silently changes which options a port defines.
fn read_freebsd_version(mnt: &Path) -> Result<String> {
    let param_h = mnt.join("usr/include/sys/param.h");
    let text = fs::read_to_string(&param_h).with_context(|| {
        format!(
            "cannot read {} — is the jail built and mounted? Pass --osversion to override",
            param_h.display()
        )
    })?;

    for line in text.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() == Some("#define") && fields.next() == Some("__FreeBSD_version") {
            if let Some(value) = fields.next() {
                return Ok(value.to_string());
            }
        }
    }

    bail!("{} defines no __FreeBSD_version", param_h.display())
}
