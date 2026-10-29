# sshenv

`sshenv host` logs in like `ssh host`, but brings your environment along:
aliases, vimrc, your ssh agent, and per-repo git accounts. Nothing is
installed, configured or left behind on the remote. A stock sshd is enough,
and other people using the same machine, login or repos see no change.

There are two pieces, both small enough to read in one sitting:

| piece | runs on | size | job |
|---|---|---|---|
| `src/main.rs` → `sshenv` | your laptop only | ~400 lines, std only | connection, upload, cleanup |
| `env/bin/git-account` | laptop **and** remotes | ~200 lines, POSIX sh | the right git account per repo |

`env/` is the folder that gets shipped. Install it as `~/.config/sshenv`.
`sshenv --list` shows exactly what will be sent.

## How a login works

```
laptop (sshenv)                                remote (stock sshd)
1. ssh -fN -A, ControlMaster  == one auth ==>  (you touch your key once)
2. tar of ~/.config/sshenv    -- stdin -->     mktemp -d; tar -x; print dir
   + this host's git account choices
3. ssh -t  (over the master)  ----------->     bash --rcfile $dir/bashrc -i
4. copy new account choices back, rm -rf $dir, close the master
```

* **Uses your system OpenSSH.** `ssh_config`, `known_hosts`, the agent and
  FIDO keys all behave as usual.
* **One connection.** All steps share it, so you authenticate once.
* **Private, temporary copy.** The remote copy is a fresh `700` dir under
  `$XDG_RUNTIME_DIR` (or `/tmp`), deleted on exit. If `sshenv` dies, the
  master connection closes itself after 10 s idle.
* **Your remote shell setup still loads.** The shipped `bashrc` sources
  `/etc/profile` and the remote's `~/.bashrc`, then adds, only inside your
  shell: `PATH`, a `git` function, `VIMINIT` and your aliases.
* **No local `tar` needed.** The archive is built by a ~45-line ustar writer
  in `main.rs`, so it doesn't depend on which `tar` your laptop has.

## Git accounts

Accounts live in `env/accounts` (git config syntax): a name, an email and a
public key file each.

**Nothing is written to repos or to anyone's git config.** Your shell's `git`
is a function, `git() { git-account git "$@"; }`. For each command it looks
up the repo's account and passes that account's settings to that one git
process through `GIT_CONFIG_*` environment variables:

* `user.name` and `user.email`
* `user.signingkey` and commit signing
* `core.sshCommand` with the account's key
* the `https://github.com/` → ssh rewrite

These variables take precedence over every config file, so a shared
checkout's own `user.email` can't override your account. They disappear when
the command ends.

Git run any other way (by a colleague, a cron job, an IDE, or `command git`)
is plain, normal git.

**Each repo is asked once.**

* **Asks first:** `clone`, `fetch`, `pull`, `push`, `commit`, `merge`,
  `rebase`, `cherry-pick`, `revert`, `am` and `tag`, in a repo with no
  account yet, ask before they run.
* **Never asks:** read-only commands like `status`, `log` and `diff` just run.
* **Clones:** a clone asks up front and remembers the answer for the new repo.

`git account` shows the repo's account. `git account NAME` sets or changes
it, `git account --clear` forgets it, and `git account --list` lists the
accounts.

**Where the answers are kept:** one line per repo (`ACCOUNT PATH`) in a
file that is only ever appended to:

| machine | file |
|---|---|
| laptop | `~/.local/state/sshenv/local` |
| remote, during the session | `$SSHENV_DIR/state/repos`, inside the temp dir |
| remote, saved | on the **laptop**, in `~/.local/state/sshenv/hosts/USER@HOST:PORT`, as `ssh -G` resolves the destination |

`sshenv` sends a host's file up at login and appends any new lines back at
logout, so choices survive between sessions without touching the remote.

## Install

```sh
cargo build --release
install -m 755 target/release/sshenv ~/.local/bin/
cp -a env ~/.config/sshenv                  # keeps the relative symlinks
                                            # (move an older ~/.config/sshenv aside first)
$EDITOR ~/.config/sshenv/accounts           # your personal email
```

For the same git behavior on the laptop, add to `~/.bashrc` (or `~/.zshrc`):

```sh
export PATH="$HOME/.config/sshenv/bin:$PATH"
git() { git-account git "$@"; }
```

Your laptop's `~/.gitconfig` can stay as it is. Its identity is simply
overridden in repos that have an account.

The symlinks in `env/` point at `~/.vimrc`, `~/.aliases`,
`~/.ssh/id_ed25519_sk_rk.pub` (work) and `~/.ssh/id_ed25519_sk_rk_github.pub`
(personal). Repoint them if needed.

### Migrating from the earlier versions

* **`~/.ssh/config`:** remove the `Tag env` lines, the `Match tagged env exec
  ... Include dynamic_config.conf` block, and the `github-personal` host.
  `sshenv` passes `-A` itself, so `ForwardAgent yes` can go too.
* **Old files:** delete `gen_export.sh` and `dynamic_config.conf`.
* **`~/.aliases`:** remove the `vi` alias that used `$MY_VIMRC`.
* **Previous version (repo-local choices):** remove the include of
  `~/.config/sshenv/git/config` from `~/.gitconfig`. Repos where you ran
  `git account` have `sshenv.account` and `user.*` entries in `.git/config`;
  remove them with `git config --local --remove-section sshenv` and
  `git config --local --unset user.email` (etc.).

## Security model

* **What leaves your laptop:** only `~/.config/sshenv` plus that host's
  choices file: configs, aliases, public keys, names, emails and repo
  paths. Private keys never leave the agent or security key.
* **What the remote can see:** root and anyone with the same UID (for example
  a shared `navflex` login) can read the temp dir. None of it is secret.
* **Agent forwarding** is the real exposure, as with any agent forwarding:
  while you're connected, root or same-UID users on the remote can ask your
  agent to sign. With `-sk` keys each signature needs a physical touch.
* **Trusted from the remote:** two things only.
  * The temp dir path, checked against a strict pattern before use.
  * New lines of its choices file. They're size-capped, stored, and only
    ever sent back to that same host. The laptop never runs them.
* **Commands built from strings:** a few short remote command lines (unpack,
  exec, cat, rm), all in `main.rs`. There is no `eval`, no base64 and no
  generated code.

## Requirements and limits

* **Remote:** a stock sshd (agent forwarding is on by default), a POSIX login
  shell, `bash`, `tar` and `mktemp`. For git: `git` and OpenSSH's `ssh` /
  `ssh-keygen` (8.2+ for `-sk` keys).
* **Laptop:** Linux or macOS, with Rust to build. The binary never leaves your
  machine, so it doesn't need to be static.
* **The account only applies to git run from your shell**, through the
  function. A script that calls `git` itself gets plain git, as does an IDE on
  the laptop.
* **Repos are identified by path.** Moving or re-cloning a repo to a new path
  asks again. A clone into a folder other than the current one
  (`git clone URL /elsewhere/x`) asks again on first use.
* **Git's own options:** git options that take a separate value (other than
  `-C` and `-c`), like `git --git-dir X commit`, aren't understood, so that
  command runs as plain git.
* **Dropped connections.** Choices made in a session that ends by a dropped
  connection or a hard kill aren't copied back. Such a session also skips
  cleanup, so the temp dir stays until logout (`$XDG_RUNTIME_DIR`) or `/tmp`
  cleanup.
* **Account names** are letters, digits, `-` and `_`. Every account needs a
  `key`.
* **Interactive logins only.** For `ssh host cmd`, scp and rsync, use plain
  ssh.
* **Vundle.** Your vimrc calls `vundle#begin()`, which errors on hosts
  without Vundle. Guard it with
  `if isdirectory(expand('~/.vim/bundle/Vundle.vim')) ... endif`.
