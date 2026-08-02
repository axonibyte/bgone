# bgone

**A reactive TUI ports configurator for FreeBSD.**

`bgone` modernizes the traditional `make config` workflow. It indexes FreeBSD port Makefiles into a local SQLite database and presents every port in a build — targets and dependencies alike — as one flat, alphabetised list you can navigate and configure in a single pass.

---

## Why bgone?

The classic `dialog`-based `make config` interface has served FreeBSD well for decades. However, when building software with complex or multi-tiered dependency chains—like web servers, browsers, or desktop stacks—configuring options can become tedious:

* You get prompted sequentially by modal popups for each dependent port during a build.
* It is difficult to see how enabling an option on a parent port triggers options and dependencies several levels down.
* Reviewing or changing previously saved options usually means stepping back through recursive menus.

`bgone` indexes your ports tree into a local SQLite database and presents the whole build as one alphabetised list: every port appears exactly once, whether you asked for it or something else dragged it in. Relationships are shown as references you jump between rather than as nesting, so a dependency shared by five ports is one entry with one set of options, not five copies to keep in step. Toggle options, follow dependencies in either direction, and save to `/var/db/ports/` before starting a build.

---

## Features

* **Flat Port List**: Every reachable port is listed once, alphabetised by origin. No nesting to lose your place in, and no port drawn twice for you to reconcile.
* **Provenance Colours**: White for ports you named, yellow for ports pulled in. Asking for a port by name outranks having it dragged in.
* **Relationships, Both Directions**: Each port shows what its options pull in (`depends on`, under the option responsible), what it needs regardless of options (`requires`), and what pulls *it* in (`required by`, with conditional parents in yellow). `Enter` jumps to any of them; `Backspace` retraces.
* **Live Reachability**: Turn an option off and the port it pulled in leaves the list, along with anything only that port needed. Its selections stay in memory, so turning the option back on restores them rather than resetting. Ports outside the list are not written.
* **Complete Dependency Coverage**: Walks the same edges `poudriere options` does — both the option-conditional dependencies (`OPT_LIB_DEPENDS`) and the unconditional ones (`_UNIFIED_DEPENDS`: `PKG`, `EXTRACT`, `PATCH`, `FETCH`, `BUILD`, `LIB`, `RUN` and `TEST`). The walk follows chains to their end rather than stopping at a fixed depth; it terminates by reaching a port already on the list, which a finite ports tree guarantees.
* **Multiple Targets & Globs**: Configure several ports in one session by listing origins, passing shell-style patterns (`"www/py-*"`), or reading a list from a file (`-f`).
* **Option Groups & Radios**: Supports standard checkboxes (`[X]`), mutual-exclusion radio groups (`(*)`), and group categories (`<CATEGORY>`).
* **Multi-Core Parallel Indexing**: Uses `rayon` to parse Makefile dependencies concurrently across CPU cores into a local SQLite cache (`bgone_cache.db`).
* **System Option Preloading**: Reads existing configuration files from `/var/db/ports/<category>_<port>/options` and `/etc/make.conf` on startup so previously saved preferences are preserved.
* **Resolved, not guessed**: every port is evaluated by `make` at index time, so `.if`, `.for`, `MASTERDIR`, `.include` and `Mk/Uses` injection all resolve — because the ports framework resolves them. Dependency targets are foreign keys to real ports, and anything that cannot be resolved is recorded rather than dropped.
* **Option semantics**: `FOO_IMPLIES` and `FOO_PREVENTS` are enforced as you toggle, the way `bsddialog` does, so you cannot save a combination the framework would override.
* **`make config` File Format**: Writes the same `OPTIONS_FILE_SET+=` / `OPTIONS_FILE_UNSET+=` files `make config` and `poudriere options` write, listing every option the port defines — which is what stops `make config-conditional` (and so `poudriere options` and `poudriere bulk`) from re-opening the dialog. Files in the older `WITH_`/`WITHOUT_` format are still read back.
* **In-TUI Search (`/`)**: Narrows the list to ports whose origin contains what you type — `postgres`, `databases/`, `py-`. Options are not searched, and a matching port is shown whole rather than reduced to the rows that matched. `Up`/`Down` (and `PgUp`/`PgDn`) move through the results while the bar is still open, so you can look before committing; `Enter` keeps the filter and the header reports it, `Esc` clears it. Filtering is a view: a hidden port keeps its options and is still written on save.
* **Sticky State Engine**: Expanding (`=`/`+`) or collapsing (`-`/`_`) nodes, sections, or the whole list preserves view preferences across state updates.
* **Familiar Controls**: Follows `bsddialog(1)` conventions—an `OK` / `Cancel` button row, `Space` to toggle, `Tab` to move focus, `Esc` to cancel—with a confirmation prompt guarding unsaved changes.
* **Dry-Run Output**: Preview the exact files and flags that would be written before making changes on disk.

---

## Installation

### From Source

Requirements: **Rust 1.75+** and **Cargo**.

```bash
git clone https://bitbucket.org/your-username/bgone.git
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

### 1. Indexing the Ports Tree

Before configuring ports, index your local ports tree into the SQLite cache:

```bash
bgone index --ports-dir /usr/ports

```

If you update your ports tree (`git pull` or `portsnap`), rebuild the index with `--force`:

```bash
bgone index --ports-dir /usr/ports --force

```

`--db-path` selects which cache to write, and may be given on either side of the subcommand:

```bash
bgone index --ports-dir /usr/ports --db-path ~/.cache/bgone.db

```

A cache written by an older `bgone` whose schema no longer matches is discarded on
open, and `bgone` says so. Re-run `bgone index` to rebuild it — nothing else is
lost, since the cache only ever mirrors the ports tree.

Indexing evaluates every port with `make`, which is the slow part of using
`bgone` and the only part that needs a ports tree. Configuring afterwards reads
nothing but the cache.

That is a deliberate trade. The alternative — reading Makefiles with regexes —
is far faster but cannot evaluate them, and the gap is not small: a port's
option list can be built by a `.for` loop, inherited through `MASTERDIR`,
injected by `Mk/Uses/*.mk`, or vary by architecture. Closing those one at a time
means reimplementing `bmake`, and a port does not just evaluate its own
Makefile: it evaluates `bsd.port.mk` (5,593 lines), `bsd.options.mk` and
`Mk/Uses/*.mk` (19,981 lines across 140 files) — some 38,000 lines defining 702+
variables, with 214 `!=` assignments that shell out to `sysctl`, `uname` and
`pkg` while evaluating. So `bgone` asks the ports framework instead of imitating
it, and pays for the answer once.

One `make` invocation per port yields everything: `PKGNAME`, flavours, the
complete option list with descriptions and `SINGLE`/`MULTI`/`RADIO` grouping,
`FOO_IMPLIES`/`FOO_PREVENTS`, and both kinds of dependency with their real class.
Results are keyed on Makefile mtime, so re-indexing after a tree update
re-evaluates only what changed:

```
[+] Indexed 34954 ports (33112 unchanged), 37901 options and 95324 dependency edges in 41.20s
```

Dependencies are resolved rather than guessed. A depends entry is
`test:origin[:target]` — `bsd.port.mk` extracts the origin with
`${_UNIFIED_DEPENDS:C,([^:]*:[^:]*):?.*,\1,}` and so does `bgone`, taking the
second colon-separated field and its optional `@flavour`. Every stored edge is a
foreign key to a port row, so an edge pointing at nothing cannot be written; an
entry that names no port is recorded in `unresolved_dep` with a reason rather
than dropped, which makes "did anything fail to resolve" a query:

```
sqlite3 bgone_cache.db "SELECT reason, COUNT(*) FROM unresolved_dep GROUP BY reason"
```

### Matching the jail rather than the host

Which options a port defines can depend on the architecture
(`OPTIONS_DEFINE_${ARCH}`, `OPTIONS_EXCLUDE_${OPSYS}`), so a cache built on the
host does not necessarily describe the jail you are building for. Pass the
target's identity and the tree is evaluated as that jail:

```bash
bgone index -p /usr/ports --jail-arch aarch64 --osversion 1404000 --osrel 14.4
```

What the cache was resolved as is recorded in it.

The evaluation also runs with `PORT_DBDIR`, `__MAKE_CONF`, `OPTIONS_SET` and
`OPTIONS_UNSET` neutralised, so what lands in the cache is the port as shipped.
Your saved options and `make.conf` are read separately and applied on top —
letting them reach `make` here would bake the indexing host's configuration into
the cache and then count it twice.

### 2. Configuring Ports

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
  index  Index a local FreeBSD ports tree directory into SQLite
  help   Print this message or the help of the given subcommand(s)

Arguments:
  [ORIGIN]...  Target port origin(s) or glob pattern(s) (e.g. www/apache24 "www/py-*")

Options:
      --config <FILE>       Read settings from a TOML config file
  -d, --db-path <PATH>      Path to SQLite cache DB [default: bgone_cache.db]
  -o, --options-dir <PATH>  Directory to read/write FreeBSD option files [default: /var/db/ports]
  -m, --make-conf <PATH>    Optional path to read/export global make.conf overrides
  -n, --dry-run             Perform a dry-run without writing files to disk
  -r, --force-reset         Discard previous DB cache and rebuild schema
  -i, --ignore-missing      Warn instead of bailing out on unmatched ports (unless nothing matches)
  -f, --file <FILE>         Read target origins/patterns from a file
  -h, --help                Print help information
  -V, --version             Print version information

```

`bgone index` takes its own switches:

```text
Usage: bgone index [OPTIONS]

Options:
  -p, --ports-dir <PATH>    Path to the ports tree root [default: /usr/ports]
  -f, --force               Discard previous database cache before indexing
      --jail-arch <ARCH>    Resolve as this architecture rather than the host's
      --osversion <N>       Resolve as this OSVERSION (e.g. 1404000)
      --opsys <NAME>        Resolve as this OPSYS
      --osrel <VERSION>     Resolve as this OSREL (e.g. 14.4)
  -d, --db-path <PATH>      Path to SQLite cache DB [default: bgone_cache.db]

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

[groups]
php-extensions = ["lang/php83-extensions", "lang/php84-extensions"]
```

Keys mirror the long option names with dashes turned into underscores, so
`--options-dir` is `options_dir`. `ports_dir` belongs to `bgone index` but is
written at the top level like the rest.

Precedence runs **defaults → config → arguments you actually typed**. A setting
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
| **`o`** / **`c`** | Press `OK` / `Cancel` directly |
| **`/`** or **`Ctrl + F`** | Open search / filter bar |
| **`Ctrl + L`** | Recenter the cursor row and repaint (see below) |
| **`Ctrl + R`** | Repaint without moving the view |
| **`Ctrl + S`** or **`s`** | Save configuration files and exit |
| **`q`** / **`Esc`** | Exit, confirming first if there are unsaved changes |

### The Button Row

The bottom of the screen carries a `dialog`-style button row:

```text
              <  OK  >        < Cancel >
```

`Tab` cycles focus between the list and each button; `Left` / `Right` move between the buttons once the row has focus; `Enter` or `Space` presses the focused one. Each button's highlighted first character is its **letter hotkey**—press `o` for `OK` or `c` for `Cancel` from anywhere, no focus change needed. That is the same convention `dialog` uses, and it is derived from the label, so a button named `Help` would answer to `h`.

`OK` saves and exits. `Cancel`, `q`, and `Esc` all exit without saving, but if you have unsaved option changes they raise a confirmation box first:

```text
        ┌ Discard changes and quit? ───────────────┐
        │                                          │
        │      You have unsaved option changes.    │
        │                                          │
        │            < Yes >     < No >            │
        └──────────────────────────────────────────┘
```

`y` / `n` answer it directly, `Left` / `Right` / `Tab` move between the buttons, and `Esc` is the same as `No`. Toggling an option back to the value it was loaded with clears the unsaved-changes flag, so an edit you undo will not prompt.

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

171 tests across three suites: unit tests beside the code they cover, an integration suite, and a command-line suite.

Between them they cover dependency-entry resolution against `bsd.port.mk`'s own grammar, turning a `make` reply into rows, SQLite caching, graph building, live reachability, file exporting, shared state across repeated ports, group synchronisation, search filtering, key handling driven event by event (scope escalation, focus cycling, sibling navigation, field editing, unsaved-change detection), config-file precedence, and every documented command-line switch.

Tests run inside isolated temporary directories and clean up automatically on completion:

```bash
cargo test

```

---

## Tech Stack

* **Language**: Rust (2021 edition)
* **TUI Engine**: `ratatui` + `crossterm`
* **Concurrency**: `rayon`
* **Database**: `rusqlite` (bundled SQLite)
* **CLI Parser**: `clap` (v4)

---

## License

Distributed under the [BSD 2-Clause License](https://www.google.com/search?q=LICENSE).
