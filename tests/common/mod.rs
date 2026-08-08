#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// RAII wrapper to guarantee test directories and files are cleaned up on completion or panic.
pub struct TempDir {
    pub path: PathBuf,
}

impl TempDir {
    pub fn new(prefix: &str) -> Self {
        let count = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "bgone_test_{}_{}_{}",
            prefix,
            std::process::id(),
            count
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("Failed to create temporary test directory");
        Self { path }
    }

    /// Absolute path of `name` inside this directory.
    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Builds a minimal ports tree containing a single port and returns its root.
pub fn write_mock_ports_tree(temp: &TempDir) -> PathBuf {
    let ports_root = temp.join("ports");
    let nginx_dir = ports_root.join("www").join("nginx");
    fs::create_dir_all(&nginx_dir).unwrap();
    fs::write(
        nginx_dir.join("Makefile"),
        "PORTNAME=   nginx\n\
         PORTVERSION=1.24.0\n\
         COMMENT=    Robust HTTP and reverse proxy server\n\
         OPTIONS_DEFINE=   HTTP2 DOCS\n\
         OPTIONS_DEFAULT=  HTTP2\n",
    )
    .unwrap();
    ports_root
}

// -------------------------------------------------------------- tree fixtures
//
// Every fact bgone has comes from evaluating a port with make, so a fixture is
// a ports tree and a `make` to read it with. These build both: a directory per
// port holding a `.port` description, and a stub `make` that turns one into the
// tagged reply the real thing would print.
//
// The stub honours `OPTIONS_OVERRIDE`, which is the whole point. A port can
// declare a dependency that exists *only* when an option is set — the shape
// `MYSQL_USES=mysql` takes — and no evaluation at the maintainer's defaults will
// ever mention it. Without a stub that models that, nothing here would exercise
// the reason the resolver asks make a second time.

use bgone::graph::DependencyGraph;
use bgone::oracle::Oracle;
use bgone::reader::SystemOptions;
use bgone::resolve::MakeEnv;
use std::collections::BTreeMap;

/// One line of a port's description, in the order the stub reads them.
#[derive(Default)]
struct Port {
    pkgname: Option<String>,
    /// name, default_on, group_type, group_name, description
    options: Vec<(String, bool, String, String, String)>,
    implies: Vec<(String, String)>,
    prevents: Vec<(String, String)>,
    /// class, dependency entry
    deps: Vec<(String, String)>,
    /// class, option, polarity, dependency entry
    optdeps: Vec<(String, String, String, String)>,
    /// class, option, dependency entry — contributed procedurally, so it shows
    /// up as an ordinary dependency but only while that option is set
    hidden: Vec<(String, String, String)>,
    /// A port whose directory exists but which make cannot evaluate.
    unreadable: bool,
}

/// A mock ports tree, plus the cache and stub make that go with it.
pub struct Tree {
    temp: TempDir,
    ports: BTreeMap<String, Port>,
    written: bool,
}

impl Tree {
    pub fn new(tag: &str) -> Self {
        Self {
            temp: TempDir::new(tag),
            ports: BTreeMap::new(),
            written: false,
        }
    }

    pub fn root(&self) -> PathBuf {
        self.temp.join("ports")
    }

    pub fn db_path(&self) -> PathBuf {
        self.temp.join("cache.db")
    }

    fn port(&mut self, origin: &str) -> &mut Port {
        self.written = false;
        self.ports.entry(origin.to_string()).or_default()
    }

    pub fn add_port(&mut self, origin: &str) {
        self.port(origin);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_option(
        &mut self,
        origin: &str,
        name: &str,
        default_on: bool,
        description: &str,
        group_type: &str,
        group_name: &str,
    ) {
        let port = self.port(origin);
        port.options.retain(|(n, ..)| n != name);
        port.options.push((
            name.to_string(),
            default_on,
            group_type.to_string(),
            group_name.to_string(),
            description.to_string(),
        ));
    }

    /// An edge that applies only when `opt` on `from` is set.
    pub fn add_option_dep(&mut self, from: &str, opt: &str, to: &str) {
        self.add_option_dep_with(from, opt, to, "RUN", "ON");
    }

    pub fn add_option_dep_with(
        &mut self,
        from: &str,
        opt: &str,
        to: &str,
        class: &str,
        polarity: &str,
    ) {
        if !self
            .ports
            .get(from)
            .map(|p| p.options.iter().any(|(n, ..)| n == opt))
            .unwrap_or(false)
        {
            self.add_option(from, opt, true, "", "DEFINE", "");
        }
        self.add_port(to);
        self.port(from).optdeps.push((
            class.to_string(),
            opt.to_string(),
            polarity.to_string(),
            entry_for(to),
        ));
    }

    /// An edge that applies whatever the options say.
    pub fn add_port_dep(&mut self, from: &str, to: &str) {
        self.add_port_dep_with(from, to, "LIB");
    }

    pub fn add_port_dep_with(&mut self, from: &str, to: &str, class: &str) {
        self.add_port(to);
        self.port(from)
            .deps
            .push((class.to_string(), entry_for(to)));
    }

    /// A dependency the port never names in a `${opt}_*_DEPENDS` variable, and
    /// which only exists while `opt` is set — what `${opt}_USES=` and
    /// `.if ${PORT_OPTIONS:M${opt}}` blocks produce.
    ///
    /// Invisible to any single evaluation at the maintainer's defaults, which is
    /// exactly the gap that made poudriere prompt for ports bgone never wrote.
    pub fn add_hidden_dep(&mut self, from: &str, opt: &str, to: &str) {
        self.add_port(to);
        self.port(from)
            .hidden
            .push(("LIB".to_string(), opt.to_string(), entry_for(to)));
    }

    pub fn add_implies(&mut self, origin: &str, opt: &str, implies: &str) {
        self.port(origin)
            .implies
            .push((opt.to_string(), implies.to_string()));
    }

    pub fn add_prevents(&mut self, origin: &str, opt: &str, prevents: &str) {
        self.port(origin)
            .prevents
            .push((opt.to_string(), prevents.to_string()));
    }

    /// Sets the package name make would report.
    pub fn set_pkgname(&mut self, origin: &str, pkgname: &str) {
        self.port(origin).pkgname = Some(pkgname.to_string());
    }

    /// Marks a port as one make cannot evaluate: the directory and Makefile are
    /// there, but nothing can be got out of it.
    pub fn make_unreadable(&mut self, origin: &str) {
        self.port(origin).unreadable = true;
    }

    /// Undoes [`Tree::make_unreadable`]: the tree was fixed, and the next write
    /// puts the port's description back.
    pub fn make_readable(&mut self, origin: &str) {
        self.port(origin).unreadable = false;
    }

    /// Every invocation of the stub make, one line each — the simulated-user
    /// engine's request-rate oracle. Counts cache *misses*, not asks: a hit
    /// never reaches the stub.
    pub fn stub_log(&self) -> PathBuf {
        self.temp.join("stub-make.log")
    }

    /// Writes only when the content differs, so re-running `write` does not
    /// bump mtimes. The framework's age is part of every memo key; rewriting an
    /// unchanged `Mk/bsd.port.mk` between two `oracle()` calls would invalidate
    /// the whole memo and quietly turn every remembered-reply test into a
    /// re-evaluation.
    fn write_if_changed(path: &Path, content: &str) {
        if fs::read(path).ok().as_deref() != Some(content.as_bytes()) {
            fs::write(path, content).unwrap();
        }
    }

    fn write(&mut self) {
        let root = self.root();
        for (origin, port) in &self.ports {
            let dir = root.join(origin);
            fs::create_dir_all(&dir).unwrap();
            Self::write_if_changed(&dir.join("Makefile"), "# read by the stub make\n");

            if port.unreadable {
                let _ = fs::remove_file(dir.join(".port"));
                continue;
            }

            let name = origin.split('/').nth(1).unwrap_or(origin);
            let mut out = format!(
                "pkgname\t{}\n",
                port.pkgname.clone().unwrap_or(format!("{name}-1.0"))
            );
            for (n, on, gt, gn, desc) in &port.options {
                out.push_str(&format!(
                    "opt\t{n}\t{}\t{gt}\t{gn}\t{desc}\n",
                    if *on { 1 } else { 0 }
                ));
            }
            for (a, b) in &port.implies {
                out.push_str(&format!("implies\t{a}\t{b}\n"));
            }
            for (a, b) in &port.prevents {
                out.push_str(&format!("prevents\t{a}\t{b}\n"));
            }
            for (class, entry) in &port.deps {
                out.push_str(&format!("dep\t{class}\t{entry}\n"));
            }
            for (class, opt, pol, entry) in &port.optdeps {
                out.push_str(&format!("optdep\t{class}\t{opt}\t{pol}\t{entry}\n"));
            }
            for (class, opt, entry) in &port.hidden {
                out.push_str(&format!("hidden\t{class}\t{opt}\t{entry}\n"));
            }
            Self::write_if_changed(&dir.join(".port"), &out);
        }
        // `Mk/bsd.port.mk` is what the oracle looks for to decide the tree is
        // readable, so the fixture has to have one.
        fs::create_dir_all(root.join("Mk")).unwrap();
        Self::write_if_changed(&root.join("Mk").join("bsd.port.mk"), "# stub\n");
        self.written = true;
    }

    /// Forces the tree onto disk without building an oracle, for tests that
    /// need to age files before the first session starts.
    pub fn write_out(&mut self) {
        if !self.written {
            self.write();
        }
    }

    pub fn oracle(&mut self) -> Oracle {
        if !self.written {
            self.write();
        }
        let conn = rusqlite::Connection::open(self.db_path()).unwrap();
        bgone::db::init_db(&conn, false).unwrap();
        drop(conn);

        let mut env = MakeEnv::new(self.root());
        env.make = stub_make(&self.temp.path);
        Oracle::new(env, self.db_path())
    }

    /// A port that exists in the tree but which make cannot evaluate.
    pub fn add_unevaluated_port(&mut self, origin: &str) {
        self.make_unreadable(origin);
    }

    /// The graph these ports make, from the targets named.
    pub fn graph(&mut self, targets: &[&str]) -> DependencyGraph {
        let owned: Vec<String> = targets.iter().map(|s| s.to_string()).collect();
        self.build(&owned, &SystemOptions::default(), false)
            .unwrap()
    }

    /// Resolves a graph the way `main` does, so a test sees exactly what a run
    /// would. Fallible, because a pattern matching nothing is an error.
    pub fn build(
        &mut self,
        patterns: &[String],
        sys_opts: &SystemOptions,
        ignore_missing: bool,
    ) -> anyhow::Result<DependencyGraph> {
        let oracle = self.oracle();
        DependencyGraph::resolve(&oracle, patterns, sys_opts, ignore_missing)
    }

    /// Asks the tree about ports whose options have just changed, exactly as
    /// the interface does off its event loop. Returns the ports that arrived.
    pub fn resettle(&mut self, graph: &mut DependencyGraph, touched: &[String]) -> Vec<String> {
        self.resettle_with(graph, touched, &SystemOptions::default())
            .arrived
    }

    /// As [`Tree::resettle`], under a saved configuration — the shape a port
    /// arriving mid-session with non-default saved options takes — and with
    /// the full outcome, failures included.
    pub fn resettle_with(
        &mut self,
        graph: &mut DependencyGraph,
        touched: &[String],
        sys_opts: &SystemOptions,
    ) -> bgone::graph::ResettleOutcome {
        let oracle = self.oracle();
        graph.resettle(&oracle, sys_opts, touched)
    }
}

/// The depends entry a port origin is named by, in `bsd.port.mk`'s own shape.
fn entry_for(origin: &str) -> String {
    let name = origin.split('/').nth(1).unwrap_or(origin);
    format!("lib{name}.so:{origin}")
}

/// A `make` that answers from a port's `.port` description.
///
/// Written out rather than mocked in-process because the resolver's contract is
/// with a *program*: it builds a command line, runs it, and parses stdout. A
/// fake that skipped the process would leave the argument construction — which
/// is where `OPTIONS_OVERRIDE` lives — untested.
pub fn stub_make(dir: &Path) -> PathBuf {
    let awk = dir.join("stub-make.awk");
    fs::write(&awk, STUB_AWK).unwrap();

    let path = dir.join("stub-make");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             portdir=''; override=''; has_override=0\n\
             while [ $# -gt 0 ]; do\n\
             \x20 case \"$1\" in\n\
             \x20   -C) shift; portdir=\"$1\" ;;\n\
             \x20   OPTIONS_OVERRIDE=*) override=\"${{1#OPTIONS_OVERRIDE=}}\"; has_override=1 ;;\n\
             \x20 esac\n\
             \x20 shift\n\
             done\n\
             printf '%s|%s|%s\\n' \"$portdir\" \"$has_override\" \"$override\" >> {log}\n\
             [ -f \"$portdir/.port\" ] || exit 1\n\
             exec awk -v override=\"$override\" -v has_override=\"$has_override\" \
             -f {awk} \"$portdir/.port\"\n",
            awk = awk.display(),
            log = dir.join("stub-make.log").display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// Turns a `.port` description into the reply `make -V` would print.
///
/// Mirrors the two things about `bsd.options.mk` that matter here: an option set
/// with `OPTIONS_OVERRIDE` replaces the maintainer's defaults outright, and a
/// set option's own `${opt}_${class}_DEPENDS` is folded into the port's
/// `${class}_DEPENDS_ALL` as though it had been unconditional all along.
const STUB_AWK: &str = r#"
BEGIN { FS = "\t"; US = sprintf("%c", 31); RS_ = sprintf("%c", 30) }

$1 == "pkgname"  { pkgname = $2 }
$1 == "opt"      { n = ++nopt; oname[n] = $2; odef[n] = $3; otype[n] = $4; ogrp[n] = $5; odesc[n] = $6 }
$1 == "implies"  { impl[$2] = impl[$2] " " $3 }
$1 == "prevents" { prev[$2] = prev[$2] " " $3 }
$1 == "dep"      { d = ++ndep; dclass[d] = $2; dentry[d] = $3 }
$1 == "optdep"   { o = ++nod; odclass[o] = $2; odopt[o] = $3; odpol[o] = $4; odentry[o] = $5 }
$1 == "hidden"   { h = ++nhid; hclass[h] = $2; hopt[h] = $3; hentry[h] = $4 }

END {
    # Which options are set: the override if one was given, else the defaults.
    if (has_override == "1") {
        split(override, want, " ")
        for (i in want) if (want[i] != "") on[want[i]] = 1
    } else {
        for (i = 1; i <= nopt; i++) if (odef[i] == "1") on[oname[i]] = 1
    }

    names = ""; defaults = ""; desc = ""; implies = ""; prevents = ""
    for (i = 1; i <= nopt; i++) {
        names = names " " oname[i]
        if (oname[i] in on) defaults = defaults " " oname[i]
        desc = desc oname[i] US odesc[i] RS_
        if (oname[i] in impl) implies = implies oname[i] US substr(impl[oname[i]], 2) RS_
        if (oname[i] in prev) prevents = prevents oname[i] US substr(prev[oname[i]], 2) RS_
        if (otype[i] != "DEFINE" && ogrp[i] != "") grp[otype[i] US ogrp[i]] = grp[otype[i] US ogrp[i]] " " oname[i]
    }

    print "PKGNAME" US pkgname
    print "OPTIONS" US names
    print "DEFAULTS" US defaults
    print "DESC" US desc
    print "IMPLIES" US implies
    print "PREVENTS" US prevents

    split("SINGLE MULTI RADIO GROUP", kinds, " ")
    for (k in kinds) {
        line = ""
        for (key in grp) {
            split(key, parts, US)
            if (parts[1] == kinds[k]) line = line parts[2] US substr(grp[key], 2) RS_
        }
        print "GRP_" kinds[k] US line
    }

    split("PKG EXTRACT PATCH FETCH BUILD LIB RUN TEST", classes, " ")
    for (c in classes) {
        cl = classes[c]

        # The port's own list, plus what bsd.options.mk folded into it: a set
        # option's declared dependencies, and anything it contributes
        # procedurally.
        all = ""
        for (i = 1; i <= ndep; i++) if (dclass[i] == cl) all = all " " dentry[i]
        for (i = 1; i <= nod; i++) {
            set = (odopt[i] in on)
            if (odclass[i] == cl && ((odpol[i] == "ON" && set) || (odpol[i] == "OFF" && !set)))
                all = all " " odentry[i]
        }
        for (i = 1; i <= nhid; i++)
            if (hclass[i] == cl && (hopt[i] in on)) all = all " " hentry[i]
        print "DEP_" cl US all

        for (p = 1; p <= 2; p++) {
            pol = (p == 1) ? "ON" : "OFF"
            byopt = ""
            for (i = 1; i <= nopt; i++) {
                entries = ""
                for (j = 1; j <= nod; j++)
                    if (odclass[j] == cl && odopt[j] == oname[i] && odpol[j] == pol)
                        entries = entries " " odentry[j]
                if (entries != "") byopt = byopt oname[i] US substr(entries, 2) RS_
            }
            print "OPTDEP_" cl "_" pol US byopt
        }
    }
}
"#;

// ------------------------------------------------------------ poudriere fixture
//
// poudriere's state is a file-per-property attribute store, so building one in
// a temp directory reproduces it exactly rather than standing in for it. What
// these write is what `poudriere jail -c` and `poudriere ports -c` write.

/// Creates `<etc>/poudriere.d` and returns the etc path to pass as
/// `--poudriere-etc`.
pub fn poudriere_etc(temp: &TempDir, name: &str) -> PathBuf {
    let etc = temp.join(name);
    fs::create_dir_all(etc.join("poudriere.d")).unwrap();
    etc
}

fn attr(etc: &Path, kind: &str, name: &str, property: &str, value: &str) {
    let dir = etc.join("poudriere.d").join(kind).join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(property), format!("{value}\n")).unwrap();
}

/// A jail, with its headers, as `poudriere jail -c` leaves it.
///
/// `arch` is written in poudriere's stored form, `host.target`.
pub fn poudriere_jail(etc: &Path, name: &str, arch: &str, version: &str, freebsd_version: &str) {
    let mnt = etc.join("jails-mnt").join(name);
    fs::create_dir_all(mnt.join("usr/include/sys")).unwrap();
    fs::write(
        mnt.join("usr/include/sys/param.h"),
        format!("#define __FreeBSD_version {freebsd_version}\n"),
    )
    .unwrap();

    attr(etc, "jails", name, "arch", arch);
    attr(etc, "jails", name, "version", version);
    attr(etc, "jails", name, "mnt", &mnt.to_string_lossy());
}

/// A jail whose metadata exists but whose filesystem does not — the shape you
/// get when a dataset is not mounted.
pub fn poudriere_jail_without_headers(etc: &Path, name: &str) {
    attr(etc, "jails", name, "arch", "amd64.amd64");
    attr(etc, "jails", name, "version", "14.4-RELEASE");
    attr(
        etc,
        "jails",
        name,
        "mnt",
        &etc.join("gone").to_string_lossy(),
    );
}

/// A ports tree. Returns the path its `mnt` points at.
pub fn poudriere_tree(etc: &Path, name: &str) -> PathBuf {
    let mnt = etc.join("ports-mnt").join(name);
    fs::create_dir_all(&mnt).unwrap();
    attr(etc, "ports", name, "mnt", &mnt.to_string_lossy());
    mnt
}
