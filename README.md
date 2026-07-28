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
* **Option Groups & Radios**: Supports standard checkboxes (`[X]`), mutual-exclusion radio groups (`(*)`), and group categories (`<CATEGORY>`).
* **Multi-Core Parallel Indexing**: Uses `rayon` to parse Makefile dependencies concurrently across CPU cores into a local SQLite cache (`bgone_cache.db`).
* **System Option Preloading**: Reads existing configuration files from `/var/db/ports/<category>_<port>/options` and `/etc/make.conf` on startup so previously saved preferences are preserved.
* **In-TUI Search (`/`)**: Filter visible tree rows by option name, description, or group name.
* **Sticky State Engine**: Expanding (`e`/`E`) or collapsing (`c`/`C`) nodes, subtrees, or the whole tree preserves view preferences across state updates.
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

### 2. Configuring a Port

To launch the TUI for a specific port origin:

```bash
bgone www/apache24

```

### Command-Line Options

```text
Usage: bgone [OPTIONS] [ORIGIN] [COMMAND]

Commands:
  index  Index a local FreeBSD ports tree directory into SQLite

Arguments:
  [ORIGIN]  Target port origin (e.g., www/apache24, databases/postgresql16-server)

Options:
  -d, --db-path <PATH>      Path to SQLite cache DB [default: bgone_cache.db]
  -o, --options-dir <PATH>  Directory to read/write FreeBSD option files [default: /var/db/ports]
  -m, --make-conf <PATH>    Optional path to read/export global make.conf overrides
  -n, --dry-run             Perform a dry-run without writing files to disk
  -f, --force-reset         Discard previous DB cache and rebuild schema
  -h, --help                Print help information

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

---

## Keybindings Cheat Sheet

| Key | Action |
| --- | --- |
| **`/`** | Open search / filter bar |
| **`Space`** | Toggle selected option or switch radio selection |
| **`Enter`** | Toggle expansion of a single port or option node |
| **`e`** | Expand the entire subtree under cursor |
| **`c`** | Collapse the entire subtree under cursor |
| **`Shift + E`** | Expand all nodes globally |
| **`Shift + C`** | Collapse all nodes globally |
| **`Up` / `Down**` | Navigate tree rows |
| **`s`** | Save configuration files and exit |
| **`q`** / **`Esc`** | Exit without saving |

---

## Testing

`bgone` includes an integration test suite covering Makefile parsing, SQLite caching, graph building, and file exporting.

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
