# bgone

**A reactive TUI ports configurator for FreeBSD.**

`bgone` modernizes the traditional `make config` workflow. It evaluates FreeBSD ports with `make` — the same computation `poudriere` does — and presents every port in a build, targets and dependencies alike, as one flat, alphabetised list you can navigate and configure in a single pass.

---

## Why bgone?

The classic `dialog`-based `make config` interface has served FreeBSD well for decades. However, when building software with complex or multi-tiered dependency chains—like web servers, browsers, or desktop stacks—configuring options can become tedious:

* You get prompted sequentially by modal popups for each dependent port during a build.
* It is difficult to see how enabling an option on a parent port triggers options and dependencies several levels down.
* Reviewing or changing previously saved options usually means stepping back through recursive menus.

`bgone` asks your ports tree, with `make`, what each port is and what it needs, and presents the whole build as one alphabetised list: every port appears exactly once, whether you asked for it or something else dragged it in. Relationships are shown as references you jump between rather than as nesting, so a dependency shared by five ports is one entry with one set of options, not five copies to keep in step. Toggle options, follow dependencies in either direction, and save to `/var/db/ports/` before starting a build.

---

## Features

* **Flat Port List**: Every reachable port is listed once, alphabetised by origin. No nesting to lose your place in, and no port drawn twice for you to reconcile.
* **Provenance Colours**: White for ports you named, yellow for ports pulled in. Asking for a port by name outranks having it dragged in.
* **Relationships, Both Directions**: Each port shows what its options pull in (`depends on`, under the option responsible), what it needs regardless of options (`requires`), and what pulls *it* in (`required by`, with conditional parents in yellow). `Enter` jumps to any of them; `Backspace` retraces.
* **Live Reachability**: Turn an option off and the port it pulled in leaves the list, along with anything only that port needed. Its selections stay in memory, so turning the option back on restores them rather than resetting. Ports outside the list are not written.
* **Complete Dependency Coverage**: Walks the same edges `poudriere options` does — both the option-conditional dependencies (`OPT_LIB_DEPENDS`) and the unconditional ones (`_UNIFIED_DEPENDS`: `PKG`, `EXTRACT`, `PATCH`, `FETCH`, `BUILD`, `LIB`, `RUN` and `TEST`). The walk follows chains to their end rather than stopping at a fixed depth; it terminates by reaching a port already on the list, which a finite ports tree guarantees.
* **Multiple Targets & Globs**: Configure several ports in one session by listing origins, passing shell-style patterns (`"www/py-*"`), or reading a list from a file (`-f`).
* **Option Groups & Radios**: Supports standard checkboxes (`[X]`), mutual-exclusion radio groups (`(*)`), and group categories (`<CATEGORY>`), listed together rather than scattered through the port's options by name. `OPTIONS_SINGLE` keeps exactly one member set; `OPTIONS_RADIO`, the optional form, also clears.
* **No Index to Build**: Ports are evaluated when they are needed, in parallel across cores, and the reply is memoised against the exact question — port, target, Makefile age, option set. The first run over a build set costs seconds to a minute; every run after it is instant. `bgone index` exists only to pay that cost up front.
* **System Option Preloading**: Reads existing configuration files from `/var/db/ports/<category>_<port>/options` and `/etc/make.conf` on startup so previously saved preferences are preserved.
* **Resolved, not guessed**: every port is evaluated by `make`, so `.if`, `.for`, `MASTERDIR`, `.include` and `Mk/Uses` injection all resolve — because the ports framework resolves them. Anything that cannot be resolved is reported rather than dropped.
* **Asked Again When It Matters**: a dependency added by `${opt}_USES` or an `.if ${PORT_OPTIONS:MFOO}` block does not exist until the option is set, so no single evaluation can find it. Turning an option on re-evaluates that port with exactly the options you chose, off the event loop, and anything new it names joins the list — configurable in the same session. This is what makes the set `bgone` writes equal the set `poudriere` computes.
* **Option semantics**: `FOO_IMPLIES` and `FOO_PREVENTS` are enforced as you toggle, the way `bsddialog` does, so you cannot save a combination the framework would override.
* **`make config` File Format**: Writes the same `OPTIONS_FILE_SET+=` / `OPTIONS_FILE_UNSET+=` files `make config` and `poudriere options` write, listing every option the port defines — which is what stops `make config-conditional` (and so `poudriere options` and `poudriere bulk`) from re-opening the dialog. Files in the older `WITH_`/`WITHOUT_` format are still read back.
* **In-TUI Search (`/`)**: Narrows the list to ports whose origin contains what you type — `postgres`, `databases/`, `py-`. Options are not searched, and a matching port is shown whole rather than reduced to the rows that matched. `Up`/`Down` (and `PgUp`/`PgDn`) move through the results while the bar is still open, so you can look before committing; `Enter` keeps the filter and the header reports it, `Esc` clears it. Filtering is a view: a hidden port keeps its options and is still written on save.
* **Sticky State Engine**: Expanding (`=`/`+`) or collapsing (`-`/`_`) nodes, sections, or the whole list preserves view preferences across state updates.
* **Familiar Controls**: Follows `bsddialog(1)` conventions—an `OK` / `Cancel` button row, `Space` to toggle, `Tab` to move focus, `Esc` to cancel. Leaving always confirms, offering `Save and exit` / `Discard` / `Cancel`; `Ctrl + S` writes the files out without leaving.
* **Hide What Has No Choice (`Shift + H`)**: Most of a build set is leaf libraries defining no options at all. Hiding them leaves only the ports there is a decision to make about. A port `make` could not read is never hidden—its options are unknown rather than absent.
* **Dry-Run Output**: Preview the exact files and flags that would be written before making changes on disk.

---

## Installation

### From Source

Requirements: **Rust 1.85+** and **Cargo**.

```bash
git clone https://bitbucket.org/axonibyte/bgone.git
cd bgone
cargo build --release

```

The compiled binary will be located at `target/release/bgone`.

### From FreeBSD Ports

*(Once committed to the ports tree)*

```bash
cd /usr/ports/ports-mgmt/bgone
make install clean

```

Or via `pkg`:

```bash
pkg install bgone

```

---

## Usage

### 1. Point it at a ports tree

There is no index to build. `bgone` learns everything by evaluating ports with
`make`, at the moment it needs to know:

```bash
bgone -o /var/db/ports -f my-ports.txt
```

`--ports-dir` says where the tree is (default `/usr/ports`). It is needed to
*configure*, not only to preheat, because what a port depends on is a question
only the tree can answer.

**If you are building with poudriere, name the jail and let `bgone` read the
rest.** Which options a port defines can depend on the architecture and OS
version, so evaluating as the host does not necessarily describe the jail you
are building for:

```bash
bgone --poudriere-jail freebsd_14-4x64 --poudriere-ports HEAD -f my-ports.txt
```

That reads poudriere's own configuration for the architecture (`amd64`), the OS
release (`14.4`), the `__FreeBSD_version` (`1404000`), the ports tree path and
the options directory — so none of them can disagree with the jail the way a
hand-copied value can.

Nothing invokes `poudriere`; its configuration is a file-per-property store
under `/usr/local/etc/poudriere.d`, and `bgone` reads it directly. Use
`--poudriere-etc` if yours lives elsewhere (`poudriere -e`). Naming a jail that
does not exist is an error listing the ones that do.

You can still spell any of it out — see [Matching the jail rather than the
host](#matching-the-jail-rather-than-the-host) — and an explicit value always
beats a derived one.

### 2. The cache is a memo, not a model

`bgone_cache.db` holds one table. Each row is a reply `make` gave, against the
question that produced it:

| origin | target | mtime | options_key | reply |
| --- | --- | --- | --- | --- |
| `mail/sqlgrey` | `ARCH=amd64 OSVERSION=1404000` | 1738... | `` | `PKGNAME…` |
| `mail/sqlgrey` | `ARCH=amd64 OSVERSION=1404000` | 1738... | ` MYSQL` | `PKGNAME…` |

Everything that could make an answer wrong is in the key, so staleness stops
being a concept. Update the tree and the mtime changes, so the old row is simply
never reached. Configure for a different jail and the target changes, so is
that one. There is nothing to invalidate and nothing to migrate; a miss costs a
single evaluation.

What that is worth, measured on `mail/sqlgrey` against a real tree (8 cores):

| | cold | warm |
| --- | --- | --- |
| resolve the 70-port closure | 8.6 s | 21 ms |
| turn MYSQL on: 870 more ports arrive | 55 s | 0.44 s |

The first run over a set is the slow part, and it is the only slow part.

Nothing about the ports domain is stored. `bgone` asks make a question and
remembers the answer verbatim; parsing happens on the way out.

This replaced a schema that modelled the tree — tables for ports, options,
dependency edges, implications, flavours — indexed once per port at the
maintainer's defaults. **That could not be made right**, and the failure was
not subtle: `poudriere options` kept prompting for ports `bgone` had never
written. See [Why one evaluation is not enough](#why-one-evaluation-is-not-enough).

### 3. Preheating, if you want to

`bgone index` is optional. It runs the same evaluations up front so a later
session starts warm:

```bash
bgone index -f my-ports.txt        # the ports you are about to configure
bgone index --all                  # the whole tree; ~35,000 evaluations
```

`--force` empties the memo first. Nothing is lost by that — it refills itself.

### Why one evaluation is not enough

A port's dependencies are a function of its options, and `bsd.options.mk` lets a
port express that two ways:

```make
MPI_LIB_DEPENDS=  libmpich.so:net/mpich     # declarative — visible to anyone
MYSQL_USES=       mysql                     # procedural — visible to nobody
```

The first names its dependency in a variable, which any evaluation reports. The
second adds `USES=mysql`, and `Mk/Uses/mysql.mk` adds a `LIB_DEPENDS` — but only
when `MYSQL` is set *while make is reading the Makefile*. Evaluate `mail/sqlgrey`
at its defaults and there is no MySQL client anywhere in the answer; force the
option on and one appears:

```
$ bmake -V '${LIB_DEPENDS_ALL}' OPTIONS_OVERRIDE=
                                             (nothing)
$ bmake -V '${LIB_DEPENDS_ALL}' OPTIONS_OVERRIDE=MYSQL
libmysqlclient.so.24:databases/mysql84-client
```

850 Makefiles in the tree use the `.if ${PORT_OPTIONS:M...}` form, which is the
same problem. No schema can hold this: the answer space is exponential in the
option count.

So `bgone` asks again. `bsd.options.mk:301` special-cases `OPTIONS_OVERRIDE` —
it replaces `PORT_OPTIONS` outright, ahead of the maintainer's defaults,
`make.conf`, the per-port variables and any saved options file — which makes
"evaluate this port with exactly these options" a single, exact question.

Turn an option on and the port is asked again, off the event loop; anything new
it names is added to the list and asked about in turn. The set `bgone` writes is
then the set `poudriere` will compute, because it is the same computation.

### What one evaluation does give

One `make` invocation per port yields `PKGNAME`, flavours, the complete option
list with descriptions and `SINGLE`/`MULTI`/`RADIO` grouping,
`FOO_IMPLIES`/`FOO_PREVENTS`, and both kinds of dependency with their real class.

Reimplementing that was considered and rejected. A port does not just evaluate
its own Makefile: it evaluates `bsd.port.mk` (5,593 lines), `bsd.options.mk` and
`Mk/Uses/*.mk` (19,981 lines across 140 files) — some 38,000 lines defining 702+
variables, with 214 `!=` assignments that shell out to `sysctl`, `uname` and
`pkg` while evaluating. So `bgone` asks the ports framework instead of imitating
it.

Dependencies are resolved rather than guessed. A depends entry is
`test:origin[:target]` — `bsd.port.mk` extracts the origin with
`${_UNIFIED_DEPENDS:C,([^:]*:[^:]*):?.*,\1,}` and so does `bgone`, taking the
second colon-separated field and its optional `@flavour`. An entry that names no
port is reported rather than dropped.

A set option's own `${opt}_${class}_DEPENDS` is folded by `bsd.options.mk` into
the port's `${class}_DEPENDS_ALL`, so make reports it twice — once plainly and
once against its option. `bgone` subtracts exactly what was folded in, keyed on
the polarity each option was evaluated at. Recording both listed the dependency
twice and, worse, kept the port in the build after the option that asked for it
had been turned off.


### Matching the jail rather than the host

Which options a port defines can depend on the architecture
(`OPTIONS_DEFINE_${ARCH}`, `OPTIONS_EXCLUDE_${OPSYS}`), so evaluating as the host
does not necessarily describe the jail you are building for. Pass the target's
identity and the tree is evaluated as that jail:

```bash
bgone -p /usr/ports --jail-arch aarch64 --osversion 1404000 --osrel 14.4 -f ports.txt
```

The target is part of every memo key, so answers for one jail can never be
mistaken for another's. Switching jails simply misses and re-evaluates; there is
nothing to invalidate.

Evaluations run with `PORT_DBDIR`, `__MAKE_CONF`, `OPTIONS_SET` and
`OPTIONS_UNSET` neutralised, so the baseline is the port as it ships. Your saved
options and `make.conf` are read separately and applied on top, and what they
come to is passed back to `make` as `OPTIONS_OVERRIDE` when it differs — letting
them reach `make` implicitly would bake this host's configuration in and then
count it twice.

### 4. Configuring Ports

To launch the TUI for a specific port origin:

```bash
bgone www/apache24

```

Several origins and shell-style patterns may be combined in a single session:

```bash
bgone www/apache24 databases/postgresql16-server "www/py-*"

```

Longer target lists can live in a file, where entries are separated by commas or any whitespace and `#` starts a comment:

```bash
bgone -f ~/my-ports.txt

```

```text
# ~/my-ports.txt
www/apache24, databases/postgresql16-server
lang/python311        # scripting
www/py-*
```

By default an origin or pattern that matches nothing aborts the run. Pass `-i` to downgrade those to warnings and continue with whatever did match; if *nothing* matches, `bgone` still exits with an error.

### Command-Line Options

```text
Usage: bgone [OPTIONS] [ORIGIN]... [COMMAND]

Commands:
  index  Fill the cache ahead of time, so a later session starts warm
  help   Print this message or the help of the given subcommand(s)

Arguments:
  [ORIGIN]...  Target port origin(s) or glob pattern(s) (e.g. www/apache24 "www/py-*")

Options:
      --config <FILE>       Read settings from a TOML config file
  -d, --db-path <PATH>      Path to SQLite cache DB [default: bgone_cache.db]
  -o, --options-dir <PATH>  Directory to read/write FreeBSD option files [default: /var/db/ports]
  -m, --make-conf <PATH>    Optional path to read/export global make.conf overrides
  -n, --dry-run             Perform a dry-run without writing files to disk
  -r, --force-reset         Empty the cache before starting
  -i, --ignore-missing      Warn instead of bailing out on unmatched ports (unless nothing matches)
  -f, --file <FILE>         Read target origins/patterns from a file
  -p, --ports-dir <PATH>    Path to the ports tree root [default: /usr/ports]
      --jail-arch <ARCH>    Resolve as this architecture rather than the host's
      --osversion <N>       Resolve as this OSVERSION (e.g. 1404000)
      --opsys <NAME>        Resolve as this OPSYS
      --osrel <VERSION>     Resolve as this OSREL (e.g. 14.4)
      --poudriere-etc <DIR>  poudriere's config root [default: /usr/local/etc]
      --poudriere-jail <JAIL>  Take arch/OS version from this jail
      --poudriere-ports <TREE> Take the tree path, and name the options dir
      --poudriere-set <SET>    The poudriere set, if you use one
      --poudriere-optionsdir <NAME>  Name the options dir outright (like -o)
  -h, --help                Print help information
  -V, --version             Print version information

```

Everything above is global, so it applies to `bgone index` as well and may be
given on either side of it. The subcommand adds only two switches of its own:

```text
Usage: bgone index [OPTIONS]

Options:
  -f, --force               Empty the cache before preheating
  -a, --all                 Preheat every port in the tree (~35,000 evaluations)

```

### Examples

Preview generated option files without modifying disk state:

```bash
bgone www/nginx -n

```

Export option overrides into a custom `make.conf` snippet alongside `/var/db/ports`:

```bash
bgone lang/python311 -m /etc/make.conf

```

Configure every Python web port that resolves, ignoring the patterns that do not:

```bash
bgone -i "www/py-*" "www/rubygem-*"

```

### Colours

Colour carries **provenance**:

| | Meaning |
| --- | --- |
| white | You named this port |
| yellow | Something else pulled it in |
| yellow, in `required by` | That parent needs this port only because an option is on |

### Configuration file

`--config` points at a TOML file holding anything you would otherwise type. There
is no default config and none is looked for; without the flag `bgone` behaves
exactly as it did before config files existed.

```toml
# Where poudriere reads options from for the HEAD tree
options_dir = "/usr/local/etc/poudriere.d/HEAD-options"
file        = "/usr/local/etc/poudriere.d/port-list"
ports_dir   = "/usr/local/etc/poudriere.d/ports/HEAD"
ignore_missing = true

# Or name the jail and let the four resolution settings come from it
poudriere_jail  = "freebsd_14-4x64"
poudriere_ports = "HEAD"

[groups]
php-extensions = ["lang/php83-extensions", "lang/php84-extensions"]
```

Keys mirror the long option names with dashes turned into underscores, so
`--options-dir` is `options_dir`. `ports_dir` belongs to `bgone index` but is
written at the top level like the rest.

Precedence runs **defaults → poudriere spec in the config → settings in the
config → poudriere spec on the command line → arguments you actually typed**.
Within each of the two sources, something written explicitly beats something
derived from a jail; between them, the command line wins. So a
`--poudriere-jail` typed at the prompt does override a `jail_arch` in the file —
naming a jail on the command line is a deliberate act. A setting
the file leaves out is simply unspecified; the file is a delta over the defaults,
never a full description of a run, and omitting something is never itself an
error. A key that is *misspelled*, on the other hand, is refused by name — a
setting that silently does nothing is much harder to notice than a failure at
startup.

### Groups

A group is a set of ports whose option choices are kept in step. Families like
`lang/php8*-extensions` are meant to be configured alike and drift apart when
maintained one at a time; setting an option on any member of a group sets it on
every member that has an option by that name.

| Key | Action |
| --- | --- |
| **`Ctrl + G`** | Add the port under the cursor to a group, or start a new one |
| **`Ctrl + G`** twice | Manage groups and membership; save them to the config |

A member with no option by that name is left alone rather than guessed at, and a
radio choice is resolved against each member's *own* group of alternatives, which
need not match the port the choice was made on. A group may name a port that is
not in the current list — it is skipped for now and kept for a run that includes
it.

A port *joining* a group takes on the choices the members already there have
made, since being in the group is the statement that it should be configured
like them. Otherwise the group would claim to be in step while its newest member
disagreed with all of it until the next toggle happened to reach it. Names the
joining port does not define are skipped, names only it has are left as they
were, and the first port in a group has nobody to copy and keeps what it had.

Every port in a group carries the group's name on its row, in cyan. Membership
changes what a keystroke does — `Space` here also moves every other member — so
it has to be visible rather than remembered.

Groups live in the `[groups]` table of the config. When `--config` names one,
joining or leaving a group writes it out immediately — there is no separate save
step. Without one, changes last for the session only, until you save from the
manager, which asks where to put a config. Either way a save rewrites only that
table, so comments and settings you put in the file by hand survive it.

### Typing in a field

The search bar and both group prompts edit alike:

| Key | Action |
| --- | --- |
| **`Left` / `Right`** | Move the caret; typing and rubbing out happen there, not at the end |
| **`Home` / `End`** | Jump to either edge |
| **`Backspace`** (or `Ctrl + H`) | Take the character behind the caret |
| **`Delete`** | Take the one in front of it |

Backspace answers to `Ctrl + H` as well, because terminals disagree about which
byte the key sends: most emit DEL, some emit BS, and crossterm reports the second
as `Ctrl + H`.

> `Ctrl + Shift + G` is deliberately not used. A terminal sends byte `0x07` for
> both it and `Ctrl + G`, so the Shift cannot be recovered — the same reason
> `+`/`_` escalate by repetition rather than by a modifier.

### Using `bgone` with `poudriere`

`poudriere` keeps port options under `${POUDRIERE_ETC}/poudriere.d/`, in a directory whose
name it derives from `-j`/`-p`/`-z`. Point `-o` at the same directory `poudriere` will read,
and `bgone` becomes a drop-in replacement for `poudriere options`:

```bash
bgone -o /usr/local/etc/poudriere.d/HEAD-options -f /usr/local/etc/poudriere.d/port-list
```

Name the jail and tree rather than indexing as the host:

```bash
bgone index --poudriere-jail freebsd_14-4x64 --poudriere-ports HEAD
bgone --poudriere-ports HEAD -f /usr/local/etc/poudriere.d/port-list
```

The second line writes to `HEAD-options`, because `bgone` composes the options
directory exactly as `poudriere options` does — the non-empty of jail, tree and
set joined with `-`, then `-options`. So `bgone` writes where `poudriere options`
with the same flags would write, and naming only the tree gives you the
tree-keyed directory discussed below. `--poudriere-set` and
`--poudriere-optionsdir` mirror `-z` and `-o`.

`bgone index` without those flags resolves the tree as the machine you run it
on, which is only right when that machine matches the jail.

At build time `poudriere` copies the **first** of these that exists into the jail's
`/var/db/ports` — it does not merge them:

| Directory | Scope |
| --- | --- |
| `<jail>-<tree>-<set>-options` | Exactly one jail, tree, and set |
| `<jail>-<set>-options` | One jail and set |
| `<jail>-<tree>-options` | One jail and tree — what `poudriere options -j <jail> -p <tree>` writes |
| `<tree>-<set>-options`, `<set>-options` | One set |
| `<tree>-options` | Every jail building from that ports tree |
| `<jail>-options` | One jail, any tree |
| `options` | Everything, when nothing above matches |

Naming the jail ties the options to a jail you will eventually replace. Keying on the
**ports tree** (`<tree>-options`, e.g. `HEAD-options`) survives jail upgrades and still
takes precedence over the bare `options` fallback. Note that a leftover
`<jail>-<tree>-options` directory outranks it — `poudriere options -j <jail> -p <tree>`
creates one unconditionally, even if you cancel out of the dialog, so remove it if you
did not mean to keep it.

---

## Keybindings Cheat Sheet

`bgone` follows `bsddialog(1)` conventions, so if your fingers already know `make config` they already know most of this.

| Key | Action |
| --- | --- |
| **`Up` / `Down`** | Navigate list rows |
| **`Shift + Up` / `Shift + Down`** | Jump to the previous / next sibling (see below) |
| **`Ctrl + Up` / `Ctrl + Down`** | Navigate five rows at a time |
| **`PgUp` / `PgDn`** | Navigate one screen at a time |
| **`Home` / `End`** | Jump to the first / last row |
| **`Space`** | Toggle selected option or switch radio selection |
| **`=`** / **`-`** | Open / close just the row under the cursor |
| **`+`** / **`_`** | Open / close that row and everything nested inside it |
| **`++`** / **`__`** | Open / close the whole list (press the key twice) |
| **`Enter`** (on a relationship row) | Jump to that port's entry in the list |
| **`Backspace`** | Retrace the last jump |
| **`Ctrl + G`** | Add the port under the cursor to a group |
| **`Ctrl + G`** twice | Manage groups and membership; save them to the config |
| **`Tab`** / **`Shift + Tab`** | Move focus between the list and the `OK` / `Cancel` buttons |
| **`Left` / `Right`** | Move between buttons while the button row has focus |
| **`Enter`** (elsewhere) | Press the focused button (`OK` while the list has focus) |
| **`o`** / **`c`** | Ask to save-and-exit / to leave, via the confirmation |
| **`/`** or **`Ctrl + F`** | Open search / filter bar |
| **`Ctrl + L`** | Recenter the cursor row and repaint (see below) |
| **`Ctrl + R`** | Repaint without moving the view |
| **`Shift + H`** | Hide the ports that define no options, and show them again |
| **`s`** | Ask to save and exit, with `Save and exit` already highlighted |
| **`Ctrl + S`** | Write the configuration files out and carry on |
| **`q`** / **`Esc`** / **`Ctrl + C`** | Leave, always confirming first |

### The Button Row

The bottom of the screen carries a `dialog`-style button row:

```text
              <  OK  >        < Cancel >
```

`Tab` cycles focus between the list and each button; `Left` / `Right` move between the buttons once the row has focus; `Enter` or `Space` presses the focused one — pressing a focused button is deliberate, so it acts immediately. The letter hotkeys `o`, `s` and `c` work from anywhere but go through the leaving confirmation instead, with `Save and exit` already highlighted for `o` and `s`: ending a session and writing files should never be one unguarded letter from the list.

`OK` saves and exits. `Cancel`, `q`, `Esc`, `Ctrl + C` and the letter hotkeys all ask on the way out:

```text
   ┌ Leaving bgone ───────────────────────────────────┐
   │                                                  │
   │        You have unsaved option changes.          │
   │                                                  │
   │  < Save and exit >   < Discard >   < Cancel >    │
   │                Esc again discards                │
   └──────────────────────────────────────────────────┘
```

`s` / `d` / `c` answer it directly, `Left` / `Right` / `Tab` move between the
buttons, and `Esc` or `Ctrl + C` a second time discards without waiting to be
pointed at one. It is asked whether or not anything has changed: both keys are
pressed by reflex and by habit from other programs, and neither should be able
to end a session of picking through options on the first press.

`Ctrl + S` writes the files out and stays where it is, so a long session's work
is not all riding on getting out cleanly at the end of it.

### Scope by Repetition

Expansion comes in three widths. Rather than holding a modifier to pick one, you **press the same key twice in a row** to widen it:

| Press | What it opens or closes | Then what? |
| --- | --- | --- |
| `=` / `-` | Just the row under the cursor | Anything nested inside keeps the shape it already had, so re-opening the row brings back the same view. Pressing again changes nothing. |
| `+` / `_` | That row and everything nested inside it, however deep | Re-opening shows the whole branch, because every level below was opened too. |
| `+ +` / `_ _` | The whole tree | The second press widens the first one. |

Concretely, on a port whose options you had already arranged: `-` folds the port away and `=` brings your arrangement straight back, while `_` folds it away and flattens everything underneath, so `+` afterwards reveals the entire branch rather than what you had before.

Any other keystroke in between breaks the run, so `+`, `Down`, `+` opens two separate branches rather than the entire tree. There is deliberately no timeout—only the intervening keystroke matters.

This is a workaround for a real limitation rather than a stylistic choice. Terminals cannot express `Ctrl` or `Ctrl + Shift` for these keys: `=` and `+` have no control code at all, `Ctrl + -` and `Ctrl + _` both transmit `0x1F`, and `Ctrl + Shift` is never distinguishable from `Ctrl` for any printable character. Repetition is the only channel available.

### Sibling Navigation

`Shift + Down` jumps to the next row at the same depth under the same parent. When the cursor is already on the last child, it falls through to the *following uncle*—the next row shallower than the cursor—so you keep moving outward instead of getting stuck. `Shift + Up` mirrors it: previous sibling, or the parent when the cursor is on the first child.

> **Note:** some terminal emulators and `tmux` configurations capture `Shift + Up` / `Shift + Down` for scrollback or text selection before an application can see them. If sibling navigation appears to do nothing, that is why—check your terminal's key bindings.

### Recentering (`Ctrl + L`)

`Ctrl + L` behaves like Emacs' `recenter-top-bottom`. The first press scrolls the cursor row to the middle of the viewport; pressing it again immediately after moves that row to the top, then to the bottom, then back to the middle. Any other keystroke ends the run, so the next `Ctrl + L` starts over at the middle. Rows near the start of the tree stay put when there is nothing left to scroll past. Every press also repaints the screen.

`Ctrl + R` repaints without touching the scroll position—useful after a stray `wall(1)` message scribbles over the display.

---

## Testing

280 tests across five suites: unit tests beside the code they cover, an integration suite, a command-line suite, a poudriere suite, and a simulated-user suite.

Between them they cover dependency-entry resolution against `bsd.port.mk`'s own grammar, turning a `make` reply into rows, SQLite caching (memoisation, framework-age invalidation, busy-cache tolerance), graph building, live reachability, mid-session re-evaluation under saved options, implication and conflict handling against SINGLE/RADIO groups, file exporting (including the managed make.conf block round-trip), shared state across repeated ports, group synchronisation, search filtering, key and paste handling driven event by event (scope escalation, focus cycling, sibling navigation, field editing, unsaved-change detection), config-file precedence, and every documented command-line switch.

Tests run inside isolated temporary directories and clean up automatically on completion:

```bash
cargo test

```

The suites above validate the resolver against a stub `make` that re-implements the framework's behaviour, which is what lets them run anywhere — including the Linux CI runners. The stub itself is validated separately: on a FreeBSD machine with a real ports tree, a differential suite asks the actual framework to disagree:

```bash
BGONE_PORTS_TREE=/usr/ports cargo test --test freebsd_tree_tests

```

### Simulated users

Scripted tests miss the bugs that need *history* — where no single operation is wrong but the accumulation is. The simulated-user suite (`tests/sim/`, plus a key-sequence tier inside `src/ui.rs`) runs seeded random action sequences against the real resolver stack: toggles, resettles, exports, session rebuilds, group operations, and adversarial interleavings — files edited behind the program's back, saved state pre-seeded for ports not yet in the build, the tree changing mid-session, a port whose `Makefile` stops answering, the cache dropped outright. A deliberately partial model and a set of history-independent invariants (each traceable to a real past defect, and each self-tested against canned broken input before the engine runs) check every step; failures print the seed and the full trace, then shrink to a minimal action tape ready to promote into a named regression.

Three fixed seeds run in every `cargo test` (a few seconds). For hunting:

```bash
BGONE_SIM_SEED=7 BGONE_SIM_ACTIONS=5000 cargo test --test simulated_user_tests   # tier A, one exact seed
BGONE_UI_SIM_SEED=7 cargo test --lib ui_fuzz                                     # key-sequence tier
BGONE_SIM_TREE=/usr/ports BGONE_SIM_ROOTS=ports-mgmt/pkg \
  cargo test --test simulated_user_tests real_tree                               # against a real tree
```

The engine's acceptance test was rediscovery: with five recent fixes individually reverted in a scratch worktree, the invariants caught every one and shrank each failure to a handful of steps. Its first honest runs also found (and led to fixing) a real adoption bug, and surfaced a set of load-time enforcement edges recorded in `tests/sim/mod.rs`'s module notes.

---

## Tech Stack

* **Language**: Rust (2021 edition)
* **TUI Engine**: `ratatui` + `crossterm`
* **Concurrency**: `rayon`
* **Database**: `rusqlite` (bundled SQLite)
* **CLI Parser**: `clap` (v4)

---

## License

Distributed under the [BSD 2-Clause License](LICENSE).
