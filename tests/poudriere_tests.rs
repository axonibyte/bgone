//! Reading poudriere's configuration, and how it feeds argument resolution.
//!
//! The fixtures build a real `poudriere.d` in a temp directory. Because
//! poudriere's state *is* a file-per-property attribute store, that reproduces
//! it rather than mocking it — the code under test does the same reads it would
//! do on a builder.

mod common;

use common::TempDir;
use std::path::{Path, PathBuf};

use bgone::config::Config;
use bgone::poudriere::Poudriere;

fn etc_with_jail(temp: &TempDir) -> PathBuf {
    let etc = common::poudriere_etc(temp, "etc");
    common::poudriere_jail(
        &etc,
        "freebsd_14-4x64",
        "amd64.amd64",
        "14.4-RELEASE",
        "1404000",
    );
    common::poudriere_tree(&etc, "HEAD");
    etc
}

// ============================================================================
// 1. READING THE ATTRIBUTE STORE
// ============================================================================

#[test]
fn a_jails_facts_come_out_in_the_forms_the_ports_framework_wants() {
    let temp = TempDir::new("pdr_jail");
    let etc = etc_with_jail(&temp);

    let facts = Poudriere::new(&etc).jail("freebsd_14-4x64").unwrap();

    // `arch` is stored host.target; what a port is built *for* is the target.
    assert_eq!(facts.arch, "amd64");
    // OSREL drops everything from the first '-', per bsd.port.mk:1152.
    assert_eq!(facts.osrel, "14.4");
    // OSVERSION is not a stored property at all — it comes from the jail's
    // own param.h, the same file bsd.port.mk reads.
    assert_eq!(facts.osversion, "1404000");
}

/// A cross-built jail stores two different halves; the second is the one that
/// matters.
#[test]
fn a_cross_jail_resolves_to_its_target_architecture() {
    let temp = TempDir::new("pdr_cross");
    let etc = common::poudriere_etc(&temp, "etc");
    common::poudriere_jail(&etc, "arm64", "amd64.aarch64", "14.4-RELEASE", "1404000");

    let facts = Poudriere::new(&etc).jail("arm64").unwrap();
    assert_eq!(
        facts.arch, "aarch64",
        "the host half is not what we build for"
    );
}

#[test]
fn a_ports_tree_resolves_to_its_mount_point() {
    let temp = TempDir::new("pdr_tree");
    let etc = common::poudriere_etc(&temp, "etc");
    let mnt = common::poudriere_tree(&etc, "HEAD");

    assert_eq!(Poudriere::new(&etc).ports_dir("HEAD").unwrap(), mnt);
}

/// Reproduced from options.sh:177 rather than chosen: the non-empty of
/// (jail, tree, set) joined with '-', then '-options'.
#[test]
fn the_options_directory_is_composed_exactly_as_poudriere_composes_it() {
    let temp = TempDir::new("pdr_optdir");
    let etc = common::poudriere_etc(&temp, "etc");
    let p = Poudriere::new(&etc);
    let d = etc.join("poudriere.d");

    assert_eq!(p.options_dir(None, None, None), d.join("options"));
    assert_eq!(p.options_dir(Some("j"), None, None), d.join("j-options"));
    // Tree alone: what survives replacing a jail.
    assert_eq!(
        p.options_dir(None, Some("HEAD"), None),
        d.join("HEAD-options")
    );
    assert_eq!(
        p.options_dir(Some("j"), Some("HEAD"), None),
        d.join("j-HEAD-options")
    );
    assert_eq!(
        p.options_dir(Some("j"), Some("HEAD"), Some("s")),
        d.join("j-HEAD-s-options")
    );
    assert_eq!(
        p.options_dir(None, Some("HEAD"), Some("s")),
        d.join("HEAD-s-options")
    );
    // The `-o` escape hatch names it outright.
    assert_eq!(p.named_options_dir("custom"), d.join("custom-options"));
}

#[test]
fn a_host_without_poudriere_is_simply_unavailable() {
    let temp = TempDir::new("pdr_absent");
    assert!(!Poudriere::new(&temp.path.join("nowhere")).is_available());
}

// ============================================================================
// 2. FAILURES
// ============================================================================

/// Naming a jail that is not there is an error, and the message says what is.
#[test]
fn an_unknown_jail_names_the_ones_that_exist() {
    let temp = TempDir::new("pdr_nojail");
    let etc = etc_with_jail(&temp);

    let err = Poudriere::new(&etc).jail("typo").unwrap_err().to_string();
    assert!(err.contains("typo"), "{err}");
    assert!(
        err.contains("freebsd_14-4x64"),
        "should list what is configured: {err}"
    );
}

#[test]
fn an_unknown_ports_tree_names_the_ones_that_exist() {
    let temp = TempDir::new("pdr_notree");
    let etc = etc_with_jail(&temp);

    let err = Poudriere::new(&etc)
        .ports_dir("nope")
        .unwrap_err()
        .to_string();
    assert!(err.contains("nope") && err.contains("HEAD"), "{err}");
}

/// A jail recorded but not mounted. OSVERSION could be computed back from the
/// release, but inferring it would be a guess, and a wrong OSVERSION silently
/// changes which options a port defines.
#[test]
fn a_jail_without_readable_headers_is_an_error_naming_the_file() {
    let temp = TempDir::new("pdr_nohdr");
    let etc = common::poudriere_etc(&temp, "etc");
    common::poudriere_jail_without_headers(&etc, "unbuilt");

    let err = Poudriere::new(&etc)
        .jail("unbuilt")
        .unwrap_err()
        .to_string();
    assert!(err.contains("param.h"), "{err}");
    assert!(
        err.contains("--osversion"),
        "should say how to proceed: {err}"
    );
}

// ============================================================================
// 3. PRECEDENCE
//
// Five tiers: typed argument, argument-derived, config setting, config-derived,
// default. Exercised through `resolve_from`, the same door `parse_with_config`
// uses, with --poudriere-etc pointing at the fixture.
// ============================================================================

fn index_with(etc: &Path, extra: &[&str], config: Option<&Config>) -> bgone::cli::Cli {
    let mut args: Vec<String> = vec![
        "bgone".into(),
        "--poudriere-etc".into(),
        etc.to_string_lossy().to_string(),
        "index".into(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    bgone::cli::resolve_from(args, config).unwrap()
}

/// The resolution target and the tree, which are top-level settings now: the
/// configure run needs them just as much as a preheat does.
fn index_fields(cli: &bgone::cli::Cli) -> (Option<String>, Option<String>, PathBuf) {
    (
        cli.jail_arch.clone(),
        cli.osversion.clone(),
        cli.ports_dir.clone(),
    )
}

/// Tier 2 over tier 5: naming a jail fills in what you would otherwise type.
#[test]
fn naming_a_jail_supplies_the_resolution_target() {
    let temp = TempDir::new("pdr_p2");
    let etc = etc_with_jail(&temp);

    let cli = index_with(
        &etc,
        &[
            "--poudriere-jail",
            "freebsd_14-4x64",
            "--poudriere-ports",
            "HEAD",
        ],
        None,
    );
    let (arch, osversion, ports_dir) = index_fields(&cli);

    assert_eq!(arch.as_deref(), Some("amd64"));
    assert_eq!(osversion.as_deref(), Some("1404000"));
    assert_eq!(ports_dir, etc.join("ports-mnt").join("HEAD"));
}

/// Tier 1 over tier 2: an explicit argument beats what the jail says.
#[test]
fn a_typed_argument_beats_the_jail_it_would_have_come_from() {
    let temp = TempDir::new("pdr_p1");
    let etc = etc_with_jail(&temp);

    let cli = index_with(
        &etc,
        &[
            "--poudriere-jail",
            "freebsd_14-4x64",
            "--jail-arch",
            "i386",
            "--osversion",
            "1300000",
        ],
        None,
    );
    let (arch, osversion, _) = index_fields(&cli);

    assert_eq!(arch.as_deref(), Some("i386"));
    assert_eq!(osversion.as_deref(), Some("1300000"));
}

/// Tier 3 over tier 4: within the config file, what is written explicitly beats
/// what its own poudriere spec derives.
#[test]
fn an_explicit_config_setting_beats_the_configs_own_poudriere_spec() {
    let temp = TempDir::new("pdr_p34");
    let etc = etc_with_jail(&temp);

    let config = Config::parse(&format!(
        "poudriere_etc = '{}'\npoudriere_jail = 'freebsd_14-4x64'\njail_arch = 'powerpc64'\n",
        etc.display()
    ))
    .unwrap();

    let cli = bgone::cli::resolve_from(vec!["bgone", "index"], Some(&config)).unwrap();
    let (arch, osversion, _) = index_fields(&cli);

    assert_eq!(arch.as_deref(), Some("powerpc64"), "written beats derived");
    // Untouched by the explicit setting, so it still comes from the jail.
    assert_eq!(osversion.as_deref(), Some("1404000"));
}

/// Tier 2 over tier 3, the one worth stating outright: a jail named on the
/// command line overrides a setting written in the config file. Naming a jail
/// at the prompt is deliberate, and command-line-beats-config is the rule that
/// stays predictable when the two interleave.
#[test]
fn a_jail_named_on_the_command_line_beats_an_explicit_config_setting() {
    let temp = TempDir::new("pdr_p23");
    let etc = etc_with_jail(&temp);

    let config = Config::parse("jail_arch = 'powerpc64'\n").unwrap();
    let cli = index_with(
        &etc,
        &["--poudriere-jail", "freebsd_14-4x64"],
        Some(&config),
    );
    let (arch, _, _) = index_fields(&cli);

    assert_eq!(arch.as_deref(), Some("amd64"));
}

/// The options directory follows the same ladder, and naming only the tree
/// gives the tree-keyed directory that survives replacing a jail.
#[test]
fn the_options_directory_resolves_through_the_same_tiers() {
    let temp = TempDir::new("pdr_opt");
    let etc = etc_with_jail(&temp);
    let d = etc.join("poudriere.d");

    let derived = bgone::cli::resolve_from(
        vec![
            "bgone",
            "--poudriere-etc",
            &etc.to_string_lossy(),
            "--poudriere-ports",
            "HEAD",
            "www/nginx",
        ],
        None,
    )
    .unwrap();
    assert_eq!(derived.options_dir, d.join("HEAD-options"));

    let typed = bgone::cli::resolve_from(
        vec![
            "bgone",
            "--poudriere-etc",
            &etc.to_string_lossy(),
            "--poudriere-ports",
            "HEAD",
            "-o",
            "/elsewhere",
            "www/nginx",
        ],
        None,
    )
    .unwrap();
    assert_eq!(typed.options_dir, PathBuf::from("/elsewhere"));
}

/// Mentioning poudriere at neither tier leaves everything exactly as it was.
#[test]
fn without_a_poudriere_spec_nothing_changes() {
    let cli = bgone::cli::resolve_from(vec!["bgone", "index"], None).unwrap();
    let (arch, osversion, ports_dir) = index_fields(&cli);

    assert!(arch.is_none());
    assert!(osversion.is_none());
    assert_eq!(ports_dir, PathBuf::from("/usr/ports"));
}

/// Asking for poudriere where there is none is refused rather than ignored.
#[test]
fn a_poudriere_argument_on_a_host_without_poudriere_is_refused() {
    let temp = TempDir::new("pdr_none");
    let err = bgone::cli::resolve_from(
        vec![
            "bgone",
            "--poudriere-etc",
            &temp.path.join("nowhere").to_string_lossy(),
            "--poudriere-jail",
            "whatever",
            "index",
        ],
        None,
    )
    .unwrap_err()
    .to_string();

    assert!(err.contains("poudriere.d"), "{err}");
}
