# bgone

**A reactive TUI ports configurator for FreeBSD.**

`bgone` modernizes the traditional `make config` workflow. It parses FreeBSD port Makefiles, maps option dependency chains into a local SQLite database, and provides an interactive tree UI to inspect and configure port options across dependency chains in real time.

---

## Why bgone?

The classic `dialog`-based `make config` interface has served FreeBSD well for decades. However, when building software with complex or multi-tiered dependency chains—like web servers, browsers, or desktop stacks—configuring options can become tedious:

* You get prompted sequentially by modal popups for each dependent port during a build.
* It is difficult to see how enabling an option on a parent port triggers options and dependencies several levels down.
* Reviewing or changing previously saved options usually means stepping back through recursive menus.

`bgone` indexes your ports tree into a local SQLite database, builds an in-memory graph of options and their sub-dependencies, and presents them in a single, navigable tree. You can expand subtrees, toggle radio choices, filter options, and save your selections to `/var/db/ports/` before starting a build.

---

## Features

* **Reactive Dependency Graph**: View port options alongside the sub-dependencies they pull in. Toggle an option, and its downstream port options update immediately.
* **Shared State for Repeated Ports**: A port that shows up more than once—as a dependency of several targets, or as both an explicit target and someone else's dependency—is a single configuration. Changing an option on one occurrence updates every other occurrence on the spot, and the port is still written out exactly once.
* **Multiple Targets & Globs**: Configure several ports in one session by listing origins, passing shell-style patterns (`"www/py-*"`), or reading a list from a file (`-f`).
* **Option Groups & Radios**: Supports standard checkboxes (`[X]`), mutual-exclusion radio groups (`(*)`), and group categories (`<CATEGORY>`).
* **Multi-Core Parallel Indexing**: Uses `rayon` to parse Makefile dependencies concurrently across CPU cores into a local SQLite cache (`bgone_cache.db`).
* **System Option Preloading**: Reads existing configuration files from `/var/db/ports/<category>_<port>/options` and `/etc/make.conf` on startup so previously saved preferences are preserved.
* **In-TUI Search (`/`)**: Filter visible tree rows by option name, description, or group name.
* **Sticky State Engine**: Expanding (`=`/`+`) or collapsing (`-`/`_`) nodes, sections, or the whole tree preserves view preferences across state updates.
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
  -p, --ports-dir <PATH>  Path to the ports tree root [default: /usr/ports]
  -f, --force             Discard previous database cache before indexing
  -d, --db-path <PATH>    Path to SQLite cache DB [default: bgone_cache.db]

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

---

## Keybindings Cheat Sheet

`bgone` follows `bsddialog(1)` conventions, so if your fingers already know `make config` they already know most of this.

| Key | Action |
| --- | --- |
| **`Up` / `Down`** | Navigate tree rows |
| **`Shift + Up` / `Shift + Down`** | Jump to the previous / next sibling (see below) |
| **`Ctrl + Up` / `Ctrl + Down`** | Navigate five rows at a time |
| **`PgUp` / `PgDn`** | Navigate one screen at a time |
| **`Home` / `End`** | Jump to the first / last row |
| **`Space`** | Toggle selected option or switch radio selection |
| **`=`** / **`-`** | Open / close just the row under the cursor |
| **`+`** / **`_`** | Open / close that row and everything nested inside it |
| **`++`** / **`__`** | Open / close the whole tree (press the key twice) |
| **`Tab`** / **`Shift + Tab`** | Move focus between the list and the `OK` / `Cancel` buttons |
| **`Left` / `Right`** | Move between buttons while the button row has focus |
| **`Enter`** | Press the focused button (`OK` while the list has focus) |
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

`bgone` includes an integration test suite covering Makefile parsing, SQLite caching, graph building, file exporting, shared state across repeated ports, key handling (scope escalation, focus cycling, sibling navigation, unsaved-change detection), and every documented command-line switch.

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
