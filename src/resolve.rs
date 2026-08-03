//! Asks the ports tree what a port actually is, by evaluating it.
//!
//! Everything here used to be guessed. The old sweep read Makefiles with regexes
//! and derived a dependency's target with
//! `([a-zA-Z0-9_\-]+/[a-zA-Z0-9_\-]+)`, taking *every* `word/word` substring in
//! a depends value as a candidate port. It was right about 99.5% of the time by
//! accident of the character class, and the junk it wrote was deleted afterwards
//! by a `DELETE ... WHERE dep_origin NOT IN (SELECT origin FROM ports)`.
//!
//! Reimplementing bmake was considered and rejected. Its *language* is bounded —
//! the tree uses ~13 of its 24 variable modifiers and a small conditional
//! grammar. Its *environment* is not: evaluating one port Makefile means
//! evaluating `bsd.port.mk` (5,593 lines), `bsd.options.mk`, and `Mk/Uses/*.mk`
//! (19,981 lines across 140 files) — some 38,000 lines defining 702+ variables
//! and containing 214 `!=` assignments that shell out to `sysctl`, `uname` and
//! `pkg` while evaluating. A faithful evaluator would be bmake plus a shell.
//!
//! So bmake evaluates, once per port, at index time. Everything a port
//! contributes — identity, options, descriptions, grouping, implications and
//! both kinds of dependency — comes out of a single `make -V` invocation, which
//! is why `.if`, `.for`, `MASTERDIR`, `.include` and `Mk/Uses` injection all
//! simply work: none of them is our problem any more.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

/// Field separator inside one emitted record.
///
/// Public so tests can build a reply without duplicating the wire format.
/// Control characters because an option description is arbitrary text — any
/// printable delimiter would eventually appear inside one.
pub const US: char = '\u{1f}';
/// Record separator between the items of a list-valued query.
pub const RS: char = '\u{1e}';

/// The dependency classes making up `_UNIFIED_DEPENDS` (`bsd.port.mk:4065`),
/// which is what `make config-recursive` — and so `poudriere options` — walks.
pub const DEP_CLASSES: [&str; 8] = [
    "PKG", "EXTRACT", "PATCH", "FETCH", "BUILD", "LIB", "RUN", "TEST",
];

/// Whether a dependency applies when its option is on or off.
///
/// `FOO_RUN_DEPENDS` applies when `FOO` is set; `FOO_RUN_DEPENDS_OFF` when it is
/// not. The old sweep could not see the `_OFF` form at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polarity {
    On,
    Off,
}

impl Polarity {
    pub fn as_str(self) -> &'static str {
        match self {
            Polarity::On => "ON",
            Polarity::Off => "OFF",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionFacts {
    pub name: String,
    pub description: String,
    /// `DEFINE`, or `SINGLE`/`MULTI`/`RADIO`/`GROUP` when the option belongs to
    /// a group.
    pub group_type: String,
    pub group_name: String,
    pub default_on: bool,
    pub implies: Vec<String>,
    pub prevents: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepEntry {
    pub origin: String,
    /// The `@flavour` suffix, which selects which build of the target is wanted.
    pub flavour: Option<String>,
    pub class: String,
    /// `None` for an unconditional dependency.
    pub via_option: Option<String>,
    pub polarity: Polarity,
}

/// A depends entry that named nothing this cache can point at.
///
/// Recorded rather than dropped: "no port fails to resolve" has to be a number
/// you can query, not a claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    pub raw_entry: String,
    pub reason: &'static str,
}

pub const REASON_MALFORMED: &str = "MALFORMED";
/// The entry has a first field and a colon, but nothing after it.
///
/// Distinct from `MALFORMED` because the cause is specific and different: the
/// port wrote `libtdb.so:${SAMBA_TDB_PORT}` and the variable is unset in this
/// configuration, so the origin expanded away. Nothing is recoverable — unlike
/// an empty `@flavour`, there is no origin left — but "a variable in this port
/// is unset" is a different thing to chase than "this line is malformed".
pub const REASON_EMPTY_ORIGIN: &str = "EMPTY_ORIGIN";
pub const REASON_ABSOLUTE: &str = "ABSOLUTE_PATH";
pub const REASON_UNEXPANDED: &str = "UNEXPANDED_VAR";
pub const REASON_NO_SUCH_PORT: &str = "NO_SUCH_PORT";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortFacts {
    pub origin: String,
    pub pkgname: String,
    pub pkgbase: String,
    pub flavours: Vec<String>,
    pub options: Vec<OptionFacts>,
    pub deps: Vec<DepEntry>,
    pub unresolved: Vec<Unresolved>,
    pub source_mtime: i64,
}

/// How to invoke the ports framework.
///
/// `arch`/`osversion`/`opsys`/`osrel` are passed on the command line, so an
/// index can be resolved *as the target jail* rather than as the host — which
/// matters because `COMPLETE_OPTIONS_LIST` varies with `OPTIONS_DEFINE_${ARCH}`
/// and `OPTIONS_EXCLUDE_${OPSYS}`.
#[derive(Debug, Clone)]
pub struct MakeEnv {
    pub ports_dir: PathBuf,
    /// The make binary. `make` on FreeBSD; overridden by tests.
    pub make: PathBuf,
    /// Extra flags placed before the targets, e.g. `-m <mkdir>`.
    pub flags: Vec<String>,
    pub arch: Option<String>,
    pub osversion: Option<String>,
    pub opsys: Option<String>,
    pub osrel: Option<String>,
    /// The poudriere jail these were read from, when they were derived rather
    /// than typed. Carried only so the cache can say where it came from.
    pub via_jail: Option<String>,
}

impl MakeEnv {
    pub fn new(ports_dir: impl Into<PathBuf>) -> Self {
        Self {
            ports_dir: ports_dir.into(),
            make: PathBuf::from("make"),
            flags: Vec::new(),
            arch: None,
            osversion: None,
            opsys: None,
            osrel: None,
            via_jail: None,
        }
    }

    fn overrides(&self) -> Vec<String> {
        let mut out = vec![format!("PORTSDIR={}", self.ports_dir.display())];
        for (key, value) in [
            ("ARCH", &self.arch),
            ("OSVERSION", &self.osversion),
            ("OPSYS", &self.opsys),
            ("OSREL", &self.osrel),
        ] {
            if let Some(v) = value {
                out.push(format!("{key}={v}"));
            }
        }
        out
    }

    /// A one-line description of what this cache was resolved as, stored in the
    /// cache metadata so the configurator can say which jail it matches.
    pub fn describe_target(&self) -> String {
        let part = |k: &str, v: &Option<String>| v.as_ref().map(|v| format!("{k}={v}"));
        let parts: Vec<String> = [
            part("ARCH", &self.arch),
            part("OSVERSION", &self.osversion),
            part("OPSYS", &self.opsys),
            part("OSREL", &self.osrel),
        ]
        .into_iter()
        .flatten()
        .collect();

        let target = if parts.is_empty() {
            "host".to_string()
        } else {
            parts.join(" ")
        };

        match &self.via_jail {
            Some(jail) => format!("{target} (poudriere jail {jail})"),
            None => target,
        }
    }
}

/// Splits one depends entry into the port it names.
///
/// This is the one piece of bmake's logic reimplemented here rather than
/// delegated, and it is reimplemented from bmake's own rule rather than guessed
/// at — `bsd.port.mk:1637`:
///
/// ```text
/// UNIFIED_DEPENDS=${_UNIFIED_DEPENDS:C,([^:]*:[^:]*):?.*,\1,:O:u:Q}
/// ```
///
/// An entry is `test:origin[:target]`. The first field is what is being looked
/// for (`libfoo.so`, `p5-Net-DNS>=1.01`, a path); the second is the origin, and
/// may carry an `@flavour`; anything after is a make target and is discarded,
/// exactly as the `:?.*` does.
///
/// An entry whose origin is absolute is refused: `bsd.port.mk:4068` warns about
/// those itself ("make sure to remove ${PORTSDIR} from it"), so they are a bug
/// in the port rather than something to interpret.
pub fn parse_dep_entry(entry: &str) -> std::result::Result<(String, Option<String>), &'static str> {
    let entry = entry.trim();
    if entry.is_empty() {
        return Err(REASON_MALFORMED);
    }

    let Some((_test, rest)) = entry.split_once(':') else {
        return Err(REASON_MALFORMED);
    };

    // `[^:]*` for the origin field: everything up to the next colon, if any.
    let origin_field = rest.split(':').next().unwrap_or("");
    if origin_field.is_empty() {
        return Err(REASON_EMPTY_ORIGIN);
    }
    if origin_field.starts_with('/') {
        return Err(REASON_ABSOLUTE);
    }
    // bmake expands everything, so a surviving reference means the port names a
    // variable that does not exist in this configuration.
    if origin_field.contains("${") || origin_field.contains("$(") {
        return Err(REASON_UNEXPANDED);
    }

    let (origin, flavour) = match origin_field.split_once('@') {
        Some((o, f)) if !o.is_empty() && !f.is_empty() => (o, Some(f.to_string())),
        // A trailing `@` with nothing after it: the port wrote
        // `devel/py-pyxdg@${PY_FLAVOR}` and `PY_FLAVOR` was empty, because the
        // option that pulls in `Mk/Uses/python.mk` is off in this evaluation.
        // The origin either side of that is still exact, so the edge is kept
        // and only the flavour is unknown — dropping it would lose a real
        // dependency over a detail nothing downstream reads.
        Some((o, f)) if !o.is_empty() && f.is_empty() => (o, None),
        Some(_) => return Err(REASON_MALFORMED),
        None => (origin_field, None),
    };

    Ok((origin.to_string(), flavour))
}

/// One `-V` expression, tagged so the reply can be matched by name rather than
/// by position — an empty variable still prints its line, but relying on that
/// would make every field depend on every other field's behaviour.
fn tagged(key: &str, expr: &str) -> String {
    format!("{key}{US}{expr}")
}

/// Every query made of a port, in one invocation.
fn queries() -> Vec<String> {
    let mut q = vec![
        tagged("PKGNAME", "${PKGNAME}"),
        tagged("PKGBASE", "${PKGBASE}"),
        tagged("FLAVORS", "${FLAVORS}"),
        tagged("OPTIONS", "${COMPLETE_OPTIONS_LIST}"),
        // `PORT_OPTIONS`, not `OPTIONS_DEFAULT`: the latter is only what the
        // maintainer wrote, while `bsd.options.mk:288` also turns on DOCS, NLS,
        // EXAMPLES and IPV6 for any port defining them. Asking for the computed
        // set is what removes the hand-kept list of implicit defaults that used
        // to live in the graph. Valid only because `resolve_one` neutralises
        // everything that would otherwise layer on top of it.
        tagged("DEFAULTS", "${PORT_OPTIONS}"),
        tagged(
            "DESC",
            &format!("${{COMPLETE_OPTIONS_LIST:@o@${{o}}{US}${{${{o}}_DESC}}{RS}@}}"),
        ),
        tagged(
            "IMPLIES",
            &format!("${{COMPLETE_OPTIONS_LIST:@o@${{o}}{US}${{${{o}}_IMPLIES}}{RS}@}}"),
        ),
        tagged(
            "PREVENTS",
            &format!("${{COMPLETE_OPTIONS_LIST:@o@${{o}}{US}${{${{o}}_PREVENTS}}{RS}@}}"),
        ),
    ];

    // Grouping. `OPTIONS_GROUP` is deliberately included: an option in a GROUP
    // is a checkbox like any other, but knowing the grouping is what lets the
    // dialog show it under its heading.
    for kind in ["SINGLE", "MULTI", "RADIO", "GROUP"] {
        q.push(tagged(
            &format!("GRP_{kind}"),
            &format!("${{OPTIONS_{kind}:@g@${{g}}{US}${{OPTIONS_{kind}_${{g}}}}{RS}@}}"),
        ));
    }

    // Unconditional dependencies, per class, in bmake's own `_ALL` form.
    for class in DEP_CLASSES {
        q.push(tagged(
            &format!("DEP_{class}"),
            &format!("${{{class}_DEPENDS_ALL}}"),
        ));
    }

    // Option-conditional dependencies. `_UNIFIED_DEPENDS` reflects whichever
    // options happen to be set during evaluation, so it cannot answer "what
    // would this option pull in"; the per-option declarations can, and they are
    // static.
    for class in DEP_CLASSES {
        for (suffix, tag) in [("", "ON"), ("_OFF", "OFF")] {
            q.push(tagged(
                &format!("OPTDEP_{class}_{tag}"),
                &format!(
                    "${{COMPLETE_OPTIONS_LIST:@o@${{o}}{US}${{${{o}}_{class}_DEPENDS{suffix}}}{RS}@}}"
                ),
            ));
        }
    }

    q
}

/// Newest mtime across a port's `Makefile*`, used to tell when cached facts have
/// gone stale.
///
/// Only the port's own directory is stat'd. A change in a master port or an
/// included fragment does not bump it — but a ports tree is updated as a whole
/// and `bgone index` re-runs across it, so the window where that matters is the
/// one where someone hand-edits a master and re-indexes.
pub fn newest_makefile_mtime(port_dir: &Path) -> Option<i64> {
    let mut newest = None;
    for entry in fs::read_dir(port_dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("Makefile") {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()?
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_secs() as i64;
        newest = Some(newest.map_or(mtime, |n: i64| n.max(mtime)));
    }
    newest
}

/// Splits make's reply into the tagged fields it was asked for.
///
/// make prints one line per `-V`, but a variable holding a newline would break
/// that, so a line without a tag is treated as a continuation of the previous
/// one rather than silently shifting every field after it.
fn split_reply(text: &str) -> BTreeMap<String, String> {
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut last: Option<String> = None;

    for line in text.lines() {
        match line.split_once(US) {
            Some((key, value)) if !key.is_empty() && !key.contains(' ') => {
                fields.insert(key.to_string(), value.to_string());
                last = Some(key.to_string());
            }
            _ => {
                if let Some(key) = &last {
                    let entry = fields.entry(key.clone()).or_default();
                    entry.push('\n');
                    entry.push_str(line);
                }
            }
        }
    }
    fields
}

/// Splits a record-separated list into `(first field, rest)` pairs.
fn records(value: &str) -> Vec<(String, String)> {
    value
        .split(RS)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .filter_map(|r| {
            r.split_once(US)
                .map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))
        })
        .collect()
}

fn words(value: &str) -> Vec<String> {
    value.split_whitespace().map(str::to_string).collect()
}

/// Turns one make reply into the facts for a port.
pub fn parse_reply(origin: &str, text: &str, source_mtime: i64) -> Result<PortFacts> {
    let f = split_reply(text);
    let get = |k: &str| f.get(k).map(String::as_str).unwrap_or("").trim();

    let pkgname = get("PKGNAME").to_string();
    if pkgname.is_empty() {
        bail!("{origin}: make reported an empty PKGNAME");
    }
    let pkgbase = match get("PKGBASE") {
        "" => pkgname
            .rsplit_once('-')
            .map(|(base, _)| base)
            .unwrap_or(&pkgname)
            .to_string(),
        base => base.to_string(),
    };

    let option_names = words(get("OPTIONS"));
    let defaults = words(get("DEFAULTS"));

    let descs: BTreeMap<String, String> = records(get("DESC")).into_iter().collect();
    let implies: BTreeMap<String, String> = records(get("IMPLIES")).into_iter().collect();
    let prevents: BTreeMap<String, String> = records(get("PREVENTS")).into_iter().collect();

    // group membership -> (type, group name), so each option can be looked up
    let mut grouping: BTreeMap<String, (String, String)> = BTreeMap::new();
    for kind in ["SINGLE", "MULTI", "RADIO", "GROUP"] {
        for (group_name, members) in records(get(&format!("GRP_{kind}"))) {
            for member in words(&members) {
                grouping.insert(member, (kind.to_string(), group_name.clone()));
            }
        }
    }

    let options: Vec<OptionFacts> = option_names
        .iter()
        .map(|name| {
            let (group_type, group_name) = grouping
                .get(name)
                .cloned()
                .unwrap_or_else(|| ("DEFINE".to_string(), String::new()));
            OptionFacts {
                name: name.clone(),
                description: descs.get(name).cloned().unwrap_or_default(),
                group_type,
                group_name,
                default_on: defaults.contains(name),
                implies: implies.get(name).map(|v| words(v)).unwrap_or_default(),
                prevents: prevents.get(name).map(|v| words(v)).unwrap_or_default(),
            }
        })
        .collect();

    let mut deps = Vec::new();
    let mut unresolved = Vec::new();

    let mut take =
        |entry: &str, class: &str, via: Option<&str>, polarity: Polarity| match parse_dep_entry(
            entry,
        ) {
            Ok((dep_origin, flavour)) => deps.push(DepEntry {
                origin: dep_origin,
                flavour,
                class: class.to_string(),
                via_option: via.map(str::to_string),
                polarity,
            }),
            Err(reason) => unresolved.push(Unresolved {
                raw_entry: entry.to_string(),
                reason,
            }),
        };

    for class in DEP_CLASSES {
        for entry in words(get(&format!("DEP_{class}"))) {
            take(&entry, class, None, Polarity::On);
        }
    }

    for class in DEP_CLASSES {
        for (suffix_tag, polarity) in [("ON", Polarity::On), ("OFF", Polarity::Off)] {
            for (opt, value) in records(get(&format!("OPTDEP_{class}_{suffix_tag}"))) {
                for entry in words(&value) {
                    take(&entry, class, Some(&opt), polarity);
                }
            }
        }
    }

    Ok(PortFacts {
        origin: origin.to_string(),
        pkgname,
        pkgbase,
        flavours: words(get("FLAVORS")),
        options,
        deps,
        unresolved,
        source_mtime,
    })
}

/// Evaluates one port.
pub fn resolve_one(env: &MakeEnv, origin: &str) -> Result<PortFacts> {
    let port_dir = env.ports_dir.join(origin);
    let source_mtime = newest_makefile_mtime(&port_dir)
        .with_context(|| format!("{origin}: no Makefile under {port_dir:?}"))?;

    let mut cmd = Command::new(&env.make);
    cmd.arg("-C").arg(&port_dir);
    for flag in &env.flags {
        cmd.arg(flag);
    }
    for query in queries() {
        cmd.arg("-V").arg(query);
    }
    for over in env.overrides() {
        cmd.arg(over);
    }

    // Resolve the port as shipped, not as this machine happens to be set up.
    //
    // `PORT_OPTIONS` is layered by `bsd.options.mk`: maintainer defaults, then
    // make.conf's OPTIONS_SET/UNSET, then the per-port ones, then any saved
    // options file under PORT_DBDIR. Every one of those is a *user* choice that
    // bgone reads separately through `reader::SystemOptions` and applies on top
    // — so letting them reach make here would bake the indexing host's
    // configuration into the cache and double-count it at display time.
    //
    // `__MAKE_CONF` pointing at nothing is the framework's own idiom for this
    // (`bsd.port.mk:1594`).
    for (key, value) in [
        ("PORT_DBDIR", "/nonexistent"),
        ("__MAKE_CONF", "/dev/null"),
        ("OPTIONS_SET", ""),
        ("OPTIONS_UNSET", ""),
    ] {
        cmd.arg(format!("{key}={value}"));
    }

    // A port that wants to prompt would hang the whole index.
    cmd.env("BATCH", "yes");

    let output = cmd
        .output()
        .with_context(|| format!("{origin}: could not run {:?}", env.make))?;

    if !output.status.success() {
        bail!(
            "{origin}: make failed: {}",
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("(no output)")
                .trim()
        );
    }

    parse_reply(
        origin,
        &String::from_utf8_lossy(&output.stdout),
        source_mtime,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------- the depends-entry grammar

    /// The shapes a depends entry actually takes, and what bmake's own rule
    /// makes of them.
    ///
    /// Each of these is a form the regex this replaced got wrong or got right by
    /// luck; the comments say which.
    #[test]
    fn a_depends_entry_resolves_to_its_second_field() {
        let origin = |e: &str| parse_dep_entry(e).unwrap().0;

        // Right before, by accident: `libpcre2-8` has no slash, so the scan
        // slid forward until it found one.
        assert_eq!(origin("libpcre2-8.so:devel/pcre2"), "devel/pcre2");

        // Wrong before: the path is full of `word/word` and every one of them
        // was taken as a port, leaving the prune to clear up.
        assert_eq!(
            origin("/usr/local/libexec/at-spi2-registryd:accessibility/at-spi2-core"),
            "accessibility/at-spi2-core"
        );

        // Version constraints belong to the first field and are not origins.
        assert_eq!(origin("p5-Net-DNS>=1.01:dns/p5-Net-DNS"), "dns/p5-Net-DNS");

        // A third field is a make target. `:?.*` discards it, and so do we.
        assert_eq!(origin("bison:devel/bison:build"), "devel/bison");

        // Lost entirely before: `+` and `.` are outside the old character class,
        // so `devel/libsigc++20` came out as `devel/libsigc` and was pruned.
        assert_eq!(
            origin("libsigc-2.0.so:devel/libsigc++20"),
            "devel/libsigc++20"
        );

        // Worse than lost: this truncated onto `audio/mpg123`, a *different*
        // real port, so the graph gained an edge that does not exist.
        assert_eq!(origin("mpg123.el>0:audio/mpg123.el"), "audio/mpg123.el");
    }

    /// An empty flavour is missing information, not a broken entry.
    #[test]
    fn a_trailing_empty_flavour_keeps_the_dependency() {
        assert_eq!(
            parse_dep_entry("pyxdg>0:devel/py-pyxdg@").unwrap(),
            ("devel/py-pyxdg".to_string(), None)
        );
    }

    #[test]
    fn a_flavour_is_kept_apart_from_the_origin() {
        assert_eq!(
            parse_dep_entry("foo:devel/py-setuptools@py311").unwrap(),
            ("devel/py-setuptools".to_string(), Some("py311".to_string()))
        );
        assert_eq!(
            parse_dep_entry("foo:devel/plain").unwrap(),
            ("devel/plain".to_string(), None)
        );
    }

    /// Refused rather than interpreted. Each of these means the port is wrong or
    /// unresolvable in this configuration, and guessing would put an edge
    /// somewhere arbitrary.
    #[test]
    fn entries_that_name_no_port_are_refused_with_a_reason() {
        // bsd.port.mk:4068 warns about this shape itself
        assert_eq!(
            parse_dep_entry("foo:/usr/ports/devel/bar"),
            Err(REASON_ABSOLUTE)
        );
        // make expands everything, so a survivor means the variable is unset
        assert_eq!(
            parse_dep_entry("foo:devel/${UNSET_VERSION}"),
            Err(REASON_UNEXPANDED)
        );
        assert_eq!(parse_dep_entry("no-colon-at-all"), Err(REASON_MALFORMED));
        // `libtdb.so:${SAMBA_TDB_PORT}` with the variable unset
        assert_eq!(parse_dep_entry("libtdb.so:"), Err(REASON_EMPTY_ORIGIN));
        // No colon at all: not a depends entry in the first place. This is what
        // a stray `\` merging two assignments produces — the swallowed
        // `JOSE_RUN_DEPENDS=` arrives as a word in the previous option's list.
        assert_eq!(parse_dep_entry("JOSE_RUN_DEPENDS="), Err(REASON_MALFORMED));
        assert_eq!(parse_dep_entry("test:@flavour"), Err(REASON_MALFORMED));
        assert_eq!(parse_dep_entry(""), Err(REASON_MALFORMED));
    }

    // ------------------------------------------------------------ reply parsing

    fn reply(fields: &[(&str, &str)]) -> String {
        fields
            .iter()
            .map(|(k, v)| format!("{k}{US}{v}\n"))
            .collect()
    }

    fn record(items: &[(&str, &str)]) -> String {
        items
            .iter()
            .map(|(a, b)| format!("{a}{US}{b}{RS}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn a_reply_becomes_options_with_their_grouping_and_defaults() {
        let text = reply(&[
            ("PKGNAME", "fontforge-20230101_2"),
            ("PKGBASE", "fontforge"),
            ("OPTIONS", "DOCS TANGO 2012 GTK3"),
            ("DEFAULTS", "DOCS TANGO GTK3"),
            (
                "DESC",
                &record(&[
                    ("TANGO", "Tango theme"),
                    // Descriptions are arbitrary text: spaces, punctuation,
                    // apostrophes. This is why the separators are control
                    // characters rather than anything printable.
                    ("GTK3", "Include freetype's internal debugger, v2"),
                ]),
            ),
            ("GRP_SINGLE", &record(&[("THEME", "TANGO 2012")])),
            ("GRP_RADIO", &record(&[("GUI", "GTK3")])),
        ]);

        let facts = parse_reply("print/fontforge", &text, 0).unwrap();
        assert_eq!(facts.pkgname, "fontforge-20230101_2");
        assert_eq!(facts.options.len(), 4);

        let opt = |n: &str| facts.options.iter().find(|o| o.name == n).unwrap();
        assert_eq!(
            opt("GTK3").description,
            "Include freetype's internal debugger, v2"
        );
        assert_eq!(opt("TANGO").group_type, "SINGLE");
        assert_eq!(opt("TANGO").group_name, "THEME");
        assert_eq!(opt("GTK3").group_type, "RADIO");
        assert_eq!(opt("DOCS").group_type, "DEFINE");
        assert!(opt("DOCS").default_on);
        // A purely numeric option name is real — textproc/rxp has `8` and `16`,
        // and print/fontforge has `2012`. The old validator rejected them.
        assert!(!opt("2012").default_on);
        assert_eq!(opt("2012").group_name, "THEME");
    }

    #[test]
    fn both_dependency_polarities_are_read_with_their_class() {
        let text = reply(&[
            ("PKGNAME", "app-1.0"),
            ("OPTIONS", "SSL"),
            ("DEP_LIB", "libpcre2-8.so:devel/pcre2"),
            ("DEP_RUN", "bash:shells/bash"),
            (
                "OPTDEP_LIB_ON",
                &record(&[("SSL", "libssl.so:security/openssl")]),
            ),
            (
                "OPTDEP_RUN_OFF",
                &record(&[("SSL", "gnutls:security/gnutls")]),
            ),
        ]);

        let facts = parse_reply("www/app", &text, 0).unwrap();
        let find = |origin: &str| facts.deps.iter().find(|d| d.origin == origin).unwrap();

        assert_eq!(find("devel/pcre2").class, "LIB");
        assert!(find("devel/pcre2").via_option.is_none());
        assert_eq!(find("shells/bash").class, "RUN");

        assert_eq!(find("security/openssl").via_option.as_deref(), Some("SSL"));
        assert_eq!(find("security/openssl").polarity, Polarity::On);

        // The `_OFF` form, invisible to the parser this replaces
        assert_eq!(find("security/gnutls").polarity, Polarity::Off);
        assert_eq!(find("security/gnutls").class, "RUN");
    }

    #[test]
    fn implications_and_conflicts_are_read() {
        let text = reply(&[
            ("PKGNAME", "nginx-1.0"),
            ("OPTIONS", "NJS STREAM HTTP DEBUG"),
            ("IMPLIES", &record(&[("NJS", "STREAM HTTP")])),
            ("PREVENTS", &record(&[("DEBUG", "STREAM")])),
        ]);

        let facts = parse_reply("www/nginx", &text, 0).unwrap();
        let opt = |n: &str| facts.options.iter().find(|o| o.name == n).unwrap();
        assert_eq!(opt("NJS").implies, vec!["STREAM", "HTTP"]);
        assert_eq!(opt("DEBUG").prevents, vec!["STREAM"]);
        assert!(opt("HTTP").implies.is_empty());
    }

    /// An entry make could not resolve is recorded, not dropped.
    #[test]
    fn unresolvable_entries_are_carried_out_of_the_reply() {
        let text = reply(&[
            ("PKGNAME", "app-1.0"),
            ("OPTIONS", ""),
            ("DEP_LIB", "libfoo.so:devel/foo garbage /abs:/usr/ports/x/y"),
        ]);

        let facts = parse_reply("www/app", &text, 0).unwrap();
        assert_eq!(facts.deps.len(), 1);
        assert_eq!(facts.deps[0].origin, "devel/foo");
        assert_eq!(facts.unresolved.len(), 2);
        assert!(facts
            .unresolved
            .iter()
            .any(|u| u.reason == REASON_MALFORMED));
        assert!(facts.unresolved.iter().any(|u| u.reason == REASON_ABSOLUTE));
    }

    #[test]
    fn an_empty_pkgname_is_an_error_rather_than_a_blank_port() {
        let text = reply(&[("PKGNAME", ""), ("OPTIONS", "A")]);
        assert!(parse_reply("www/app", &text, 0).is_err());
    }

    /// Every query is tagged, so a field that prints nothing cannot shift the
    /// meaning of the ones after it.
    #[test]
    fn fields_are_matched_by_tag_not_by_position() {
        let text = reply(&[
            ("OPTIONS", "A B"),
            ("PKGNAME", "app-1.0"),
            ("DEFAULTS", "B"),
        ]);
        let facts = parse_reply("www/app", &text, 0).unwrap();
        assert_eq!(facts.pkgname, "app-1.0");
        assert_eq!(facts.options.len(), 2);
        assert!(
            facts
                .options
                .iter()
                .find(|o| o.name == "B")
                .unwrap()
                .default_on
        );
    }

    #[test]
    fn the_resolved_target_is_named_for_the_cache_metadata() {
        let mut env = MakeEnv::new("/usr/ports");
        assert_eq!(env.describe_target(), "host");
        env.arch = Some("aarch64".into());
        env.osversion = Some("1404000".into());
        assert_eq!(env.describe_target(), "ARCH=aarch64 OSVERSION=1404000");

        // Where it came from, when it was not typed by hand
        env.via_jail = Some("freebsd_14-4x64".into());
        assert_eq!(
            env.describe_target(),
            "ARCH=aarch64 OSVERSION=1404000 (poudriere jail freebsd_14-4x64)"
        );
    }
}
