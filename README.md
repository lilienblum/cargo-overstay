![cargo-overstay — reclaim stale Rust build artifacts](assets/github-banner.webp)

# cargo-overstay

A zero-dependency tool that keeps Rust build artifacts (`target/`
directories) from filling your disk. Use it two ways, together or alone:

- **Manually:** `cargo overstay purge` deletes every build target it can
  prove is cargo's, right now.
- **Automatically (optional):** replace `cargo` with overstay's shim and it
  tracks which workspaces you use and reclaims stale `target/` dirs in the
  background — no configuration, no commands to remember.

## Install

```sh
cargo install cargo-overstay
```

This installs a single binary: `cargo-overstay`, which makes
`cargo overstay <command>` work immediately and doubles as the shim
(inert until you set it up below).

> **Upgrading from the old `overstay` crate?** Run `cargo uninstall overstay`
> first — it owns a binary also named `cargo-overstay`, so the install above
> fails until it's gone. If you had its shim, remove the old link and dir
> (`rm -rf ~/.overstay`), drop the old PATH line, and redo the shim setup
> below: the old symlink points at a binary that no longer exists, which
> silently disables automatic cleanup. Tracking state starts fresh (the data
> dir moved); it repopulates as you build.

## Manual commands

```sh
cargo overstay purge            # reclaim everything now, scanning ~ for targets
cargo overstay purge ~/work     # scan somewhere narrower
cargo overstay ls               # tracked projects, target sizes, last use
```

`purge` first deletes every target overstay already knows about, then scans
for more. A dir is deleted outright only when cargo's own markers are inside
it (`CACHEDIR.TAG`, `.rustc_info.json`, or a compiled profile) — that goes
for known targets too, so a recorded path later reused by something else is
not silently removed. A scanned `target/` additionally needs a sibling
`Cargo.toml`. Marker-less hits are listed and deleted only after one
confirmation, and a scanned `target/` without a manifest is never touched —
your JS bundler's `target/` folder is safe. Targets with a
build currently running are skipped. The scan never follows symlinks and
skips hidden dirs, `node_modules`, and `Library`.

## Optional: automatic cleanup (replacing cargo)

For hands-off maintenance, overstay hooks in through a `cargo` shim on your
`PATH`: a symlink named `cargo` that points at `cargo-overstay`, placed ahead
of the real cargo. Invoked under the `cargo` name, overstay forwards each
invocation to the real cargo and runs its own maintenance transparently. This
works in **every** shell — interactive or not — so it also covers scripts,
`make`, CI, and AI coding agents. (A shell alias would not: an alias applies
only to interactive shells, so anything an agent or script runs would bypass
overstay entirely.)

Create the shim in a dedicated directory:

```sh
mkdir -p ~/.cargo-overstay/bin
ln -sf "$(command -v cargo-overstay)" ~/.cargo-overstay/bin/cargo
```

Then put that directory first on `PATH`, in a file every shell reads — not just
interactive ones:

```sh
# bash / zsh — ~/.zshenv is read by every zsh invocation, incl. non-interactive
echo 'export PATH="$HOME/.cargo-overstay/bin:$PATH"' >> ~/.zshenv

# fish
fish_add_path ~/.cargo-overstay/bin
```

Open a new shell. Every `cargo` call — yours, a script's, CI's, or an agent's —
now routes through overstay, which finds and runs the real cargo directly
(skipping its own shim by canonical path), so there is no recursion and the
original cargo binary is never moved or renamed.

> The shim must be a **symlink**, as shown — never a wrapper script named
> `cargo`. overstay steps over its own shim by recognizing its own executable
> among the PATH candidates; a script is a different file, so a
> name-preserving wrapper gets mistaken for the real cargo and re-run in a
> loop, while a plain `exec` wrapper loses the `cargo` name and stops
> forwarding entirely. (A copy of the binary does work, but it goes silently
> stale on every upgrade — use the symlink.) Remove the shim with
> `rm ~/.cargo-overstay/bin/cargo`.

## How automatic cleanup decides what to clean

With the shim installed, overstay needs no configuration — it uses fixed,
built-in thresholds. A project's `target/` becomes eligible when any of
these hold:

- **Inactive** — not used for 30 days.
- **Too big** — `target/` exceeds 10 GiB.
- **Over budget** — total tracked cache exceeds 75 GiB; least-recently-used
  projects are evicted first.
- **Low disk** — free space on a tracked target's volume drops below 10 GiB;
  least-recently-used projects on that volume are evicted until 20 GiB is
  free.

Inactive, over-budget, and low-disk reclaims remove the whole `target/`. A
project over the 10 GiB cap is instead trimmed in place: artifacts orphaned by
dependency churn go first, then least-recently-used compilation units (tracked
via cargo's `.fingerprint` metadata and the artifacts' own read times, so a
dep compiled long ago but still used daily counts as fresh) and stale
`incremental/` caches, until the directory fits. Files overstay doesn't
recognize are never touched.

overstay never deletes the project you are currently building, a `target/`
modified in the last 10 minutes, or anything used in the last 10 minutes when
trimming — and every reclaim first takes cargo's own build lock, so a running
build makes overstay back off rather than race it. The one exception to
current-project protection: when the current project itself grows past the
10 GiB cap, it still gets the in-place trim — never removed — since an
actively rebuilt project (e.g. with `incremental = true`) can otherwise fill
the disk with nothing overstay is allowed to evict.

## Check the shim is active

```sh
command -v cargo   # or: which cargo
```

This should print `~/.cargo-overstay/bin/cargo` (the shim). If it prints
something under `~/.cargo/bin` instead, the shim isn't first on `PATH` yet.

## Uninstall

```sh
rm ~/.cargo-overstay/bin/cargo       # remove the shim (if you set it up)
# remove the PATH line you added to your shell config
cargo uninstall cargo-overstay       # remove the binary
rm -rf ~/.local/share/cargo-overstay # (macOS: ~/Library/Application Support/cargo-overstay) drop the state file
```

## Where data lives

A small text file, guarded by a sibling lock file:

- Linux: `~/.local/share/cargo-overstay/state` (honors `XDG_DATA_HOME`)
- macOS: `~/Library/Application Support/cargo-overstay/state`

(Alongside it, `state.lock` is used only to coordinate concurrent overstay
processes; it holds no data of its own.)

## License

MIT
