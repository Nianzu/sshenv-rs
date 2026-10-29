// AI GENERATED
//! sshenv: ssh to a host and bring your environment along.
//!
//!   sshenv [ssh options] destination   interactive shell with your env
//!   sshenv --list                      show exactly what would be sent
//!
//! Every step goes over ONE ssh connection, so you authenticate once:
//!   1. Start a master connection with agent forwarding.
//!   2. Send the env directory (~/.config/sshenv) as a tar stream. The remote
//!      unpacks it into a new private temp dir and prints the dir's path.
//!   3. Run an interactive bash there, using the shipped bashrc.
//!   4. Delete the temp dir and close the connection.
//!
//! The git accounts you pick for repos on a host are remembered on your
//! laptop (~/.local/state/sshenv/hosts/USER@HOST:PORT), never on the remote.
//! They go up with the env in step 2, as state/repos, and new picks are
//! copied back before step 4.
//!
//! This program runs only on your machine and drives the system `ssh`, so
//! ssh_config, known_hosts, the agent and FIDO keys work as usual. The remote
//! needs a stock sshd, a POSIX login shell, bash, tar and mktemp.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{self, Command, ExitCode, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const USAGE: &str = "usage: sshenv [ssh options] destination\n       sshenv --list";

/// Runs on the remote, in the login shell: make a private temp dir, unpack
/// the tar stream on stdin into it, print its path. XDG_RUNTIME_DIR (a
/// per-user tmpfs, wiped at logout) is preferred over /tmp when present.
const REMOTE_UNPACK: &str = r#"umask 077
d=$(mktemp -d "${XDG_RUNTIME_DIR:-/tmp}/sshenv.XXXXXXXX") || exit 1
tar -xf - -C "$d" || { rm -rf "$d"; exit 1; }
echo "$d""#;

/// If sshenv dies without cleaning up, the master connection closes itself
/// after this many idle seconds.
const MASTER_IDLE_TIMEOUT: &str = "ControlPersist=10";

/// Where the account choices sit inside the remote temp dir.
const CHOICES: &str = "state/repos";

/// Ignore choice files bigger than this (one line per repo, so plenty).
const MAX_CHOICES: usize = 1 << 20;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None | Some("-h" | "--help") => {
            println!("{USAGE}");
            Ok(0)
        }
        Some("--list") => list(),
        Some(_) => run(args),
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("sshenv: {e}");
            ExitCode::from(255)
        }
    }
}

/// `sshenv --list`: print every file that would be sent.
fn list() -> Result<u8, String> {
    let dir = env_dir()?;
    println!("{}:", dir.display());
    for e in collect(&dir)? {
        match e.data {
            None => println!("{:>8}  {}/", "", e.path),
            Some(data) => println!("{:>8}  {}", data.len(), e.path),
        }
    }
    Ok(0)
}

/// `sshenv [ssh options] destination`: the four steps described at the top.
fn run(target: Vec<String>) -> Result<u8, String> {
    // This host's account choices, sent along as state/repos.
    let choices_file = match host_choices_file(&target) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("sshenv: {e}; git account choices will not be kept after this session");
            None
        }
    };
    let choices = choices_file.as_ref().and_then(|f| fs::read(f).ok()).unwrap_or_default();

    let mut entries = collect(&env_dir()?)?;
    if entries.iter().any(|e| e.path == "state" || e.path.starts_with("state/")) {
        return Err("the env dir must not contain 'state' (sshenv uses that name)".into());
    }
    entries.push(Entry { path: "state".into(), executable: true, data: None });
    entries.push(Entry { path: CHOICES.into(), executable: false, data: Some(choices.clone()) });
    let archive = tar(&entries)?;

    let mut session = Session { target, ctl_dir: private_temp_dir()?, remote_dir: None };

    // 1. Master connection: authenticate, then go to the background (-f)
    //    without running anything (-N). Later calls reuse it.
    let status = session
        .ssh()
        .args(["-o", "ControlMaster=yes", "-o", MASTER_IDLE_TIMEOUT, "-f", "-N"])
        .args(&session.target)
        .status()
        .map_err(|e| format!("cannot run ssh: {e}"))?;
    if !status.success() {
        return Err("connection failed".into());
    }

    // 2. Send the env. The remote replies with the temp dir's path.
    let mut child = session
        .ssh()
        .arg("-T")
        .args(&session.target)
        .arg(REMOTE_UNPACK)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run ssh: {e}"))?;
    let sent = child.stdin.take().expect("stdin is piped").write_all(&archive);
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if sent.is_err() || !output.status.success() {
        return Err("unpacking the env on the remote failed".into());
    }
    let dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !is_plain_remote_dir(&dir) {
        return Err(format!("unexpected temp dir from remote: {dir:?}"));
    }
    session.remote_dir = Some(dir.clone());

    // 3. Interactive shell with the shipped bashrc. `dir` was checked above,
    //    so it is safe to put in the command line unquoted.
    let status = session
        .ssh()
        .arg("-t")
        .args(&session.target)
        .arg(format!("exec env SSHENV_DIR={dir} bash --rcfile {dir}/bashrc -i"))
        .status()
        .map_err(|e| format!("cannot run ssh: {e}"))?;

    // Keep the account choices made during the session.
    if let Some(file) = &choices_file {
        if let Err(e) = session.bring_back_choices(file, &choices) {
            eprintln!("sshenv: {e}");
        }
    }

    // 4. Cleanup happens when `session` is dropped, right after this.
    Ok(status.code().and_then(|c| u8::try_from(c).ok()).unwrap_or(255))
}

/// The laptop's file of account choices for this host, named after the
/// user, host name and port that `ssh -G` resolves the destination to.
fn host_choices_file(target: &[String]) -> Result<PathBuf, String> {
    let out = Command::new("ssh")
        .arg("-G")
        .args(target)
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run ssh: {e}"))?;
    let config = String::from_utf8_lossy(&out.stdout);
    let value = |key: &str| {
        config.lines().find_map(|l| l.strip_prefix(key)).map_or("", str::trim).to_string()
    };
    let name = format!("{}@{}:{}", value("user "), value("hostname "), value("port "));
    let plain = name.bytes().all(|b| b.is_ascii_alphanumeric() || b"@._-:".contains(&b));
    if !out.status.success() || !plain || name.starts_with('@') || name.contains("@:") {
        return Err(format!("could not name this host for saving choices ({name:?})"));
    }
    Ok(state_dir()?.join("hosts").join(name))
}

/// One connection's state. Dropping it deletes the remote temp dir, stops
/// the master connection and removes the local socket dir.
struct Session {
    /// The user's ssh options and destination, passed to every ssh call.
    target: Vec<String>,
    /// Private local dir holding the master's control socket.
    ctl_dir: PathBuf,
    /// The remote temp dir, once the env has been unpacked there.
    remote_dir: Option<String>,
}

impl Session {
    /// An ssh command that runs over the master connection. RemoteCommand=none
    /// overrides any RemoteCommand set in ssh_config for this host.
    fn ssh(&self) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.arg("-A")
            .arg("-o")
            .arg(format!("ControlPath={}", self.ctl_dir.join("ctl").display()))
            .args(["-o", "RemoteCommand=none"]);
        cmd
    }

    /// Append the account choices made during the session to `file`. The
    /// remote copy started as `sent` and git-account only appends to it, so
    /// whatever follows `sent` is new. The data is only ever sent back to the
    /// same host; the laptop never runs it.
    fn bring_back_choices(&self, file: &Path, sent: &[u8]) -> Result<(), String> {
        let Some(dir) = &self.remote_dir else { return Ok(()) };
        let out = self
            .ssh()
            .arg("-T")
            .args(&self.target)
            .arg(format!("cat {dir}/{CHOICES}"))
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run ssh: {e}"))?;
        if !out.status.success() || out.stdout.len() > MAX_CHOICES {
            return Err("could not read back git account choices".into());
        }
        let new = out
            .stdout
            .strip_prefix(sent)
            .ok_or("git account choices on the remote changed unexpectedly; not saved")?;
        if new.is_empty() {
            return Ok(());
        }
        if let Some(parent) = file.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(file)
            .and_then(|mut f| f.write_all(new))
            .map_err(|e| format!("{}: {e}", file.display()))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(dir) = &self.remote_dir {
            let _ = self
                .ssh()
                .arg("-T")
                .args(&self.target)
                .arg(format!("rm -rf {dir}"))
                .stdin(Stdio::null())
                .status();
        }
        let _ = self
            .ssh()
            .args(["-O", "exit"])
            .args(&self.target)
            .stderr(Stdio::null())
            .status();
        let _ = fs::remove_dir_all(&self.ctl_dir);
    }
}

/// The remote chooses the temp dir name. Accept only a plain absolute path
/// whose last part is `sshenv.<something>`.
fn is_plain_remote_dir(dir: &str) -> bool {
    let plain = dir.bytes().all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b));
    let name = dir.rsplit('/').next().unwrap_or("");
    plain
        && dir.starts_with('/')
        && !dir.contains("..")
        && name.starts_with("sshenv.")
        && name.len() > "sshenv.".len()
}

/// The env directory: $XDG_CONFIG_HOME/sshenv, or ~/.config/sshenv.
fn env_dir() -> Result<PathBuf, String> {
    let dir = xdg_dir("XDG_CONFIG_HOME", ".config")?.join("sshenv");
    if dir.is_dir() {
        Ok(dir)
    } else {
        Err(format!("no env directory at {}", dir.display()))
    }
}

/// Saved state: $XDG_STATE_HOME/sshenv, or ~/.local/state/sshenv.
fn state_dir() -> Result<PathBuf, String> {
    Ok(xdg_dir("XDG_STATE_HOME", ".local/state")?.join("sshenv"))
}

/// $VAR if set, else ~/DEFAULT.
fn xdg_dir(var: &str, default: &str) -> Result<PathBuf, String> {
    match std::env::var_os(var) {
        Some(d) if !d.is_empty() => Ok(PathBuf::from(d)),
        _ => Ok(PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?).join(default)),
    }
}

/// A new directory only we can access, for the control socket. Creation
/// fails if the name already exists, so it can't be pre-planted.
fn private_temp_dir() -> Result<PathBuf, String> {
    let base = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => std::env::temp_dir(),
    };
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let dir = base.join(format!("sshenv-{}-{nanos}", process::id()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// One file or directory to send. `data` is None for directories.
struct Entry {
    /// Path relative to the env dir, '/'-separated.
    path: String,
    executable: bool,
    data: Option<Vec<u8>>,
}

/// Everything under `root`, sorted, with symlinks followed so that entries
/// can be links to your real dotfiles (e.g. vimrc -> ~/.vimrc).
fn collect(root: &Path) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    walk(root, "", 0, &mut entries)?;
    Ok(entries)
}

fn walk(dir: &Path, rel: &str, depth: u32, out: &mut Vec<Entry>) -> Result<(), String> {
    if depth > 8 {
        return Err(format!("{}: nested too deep (symlink loop?)", dir.display()));
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        names.push(entry.map_err(|e| e.to_string())?.file_name());
    }
    names.sort();
    for name in names {
        let name = name.into_string().map_err(|n| format!("non-UTF-8 file name {n:?}"))?;
        let path = dir.join(&name);
        let rel = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        let meta = fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.is_dir() {
            out.push(Entry { path: rel.clone(), executable: true, data: None });
            walk(&path, &rel, depth + 1, out)?;
        } else if meta.is_file() {
            let data = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let executable = meta.permissions().mode() & 0o100 != 0;
            out.push(Entry { path: rel, executable, data: Some(data) });
        } else {
            return Err(format!("{}: not a regular file or directory", path.display()));
        }
    }
    Ok(())
}

/// A minimal ustar archive of `entries` (regular files and directories only).
/// Owner is 0/0 and modes are 0700/0600; the remote's umask applies anyway.
fn tar(entries: &[Entry]) -> Result<Vec<u8>, String> {
    let mtime = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut out = Vec::new();
    for e in entries {
        let name = match e.data {
            None => format!("{}/", e.path),
            Some(_) => e.path.clone(),
        };
        if name.len() > 100 {
            return Err(format!("path too long for tar (max 100 bytes): {name}"));
        }
        let size = e.data.as_ref().map_or(0, Vec::len);
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        octal(&mut h[100..108], if e.executable { 0o700 } else { 0o600 }); // mode
        octal(&mut h[108..116], 0); // uid
        octal(&mut h[116..124], 0); // gid
        octal(&mut h[124..136], size as u64);
        octal(&mut h[136..148], mtime);
        h[156] = if e.data.is_none() { b'5' } else { b'0' }; // type: dir or file
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        // Checksum: byte sum of the header with the checksum field as spaces.
        h[148..156].fill(b' ');
        let sum: u64 = h.iter().map(|&b| u64::from(b)).sum();
        octal(&mut h[148..155], sum);
        out.extend_from_slice(&h);
        if let Some(data) = &e.data {
            out.extend_from_slice(data);
            out.resize(out.len().next_multiple_of(512), 0);
        }
    }
    out.resize(out.len() + 1024, 0); // end of archive: two zero blocks
    Ok(out)
}

/// Write `n` into `field` as zero-padded octal followed by a NUL.
fn octal(field: &mut [u8], n: u64) {
    let digits = format!("{:0width$o}", n, width = field.len() - 1);
    field[..digits.len()].copy_from_slice(digits.as_bytes());
    field[digits.len()] = 0;
}
