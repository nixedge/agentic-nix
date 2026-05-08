use anyhow::{Context, Result};
use clap::Parser;
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

#[derive(Parser)]
#[command(
    name = "claude-jail",
    about = "Run claude in a bubblewrap sandbox with nix daemon access"
)]
struct Args {
    /// Bind path read-only at its real path inside the jail. Repeatable.
    #[arg(long = "ro", value_name = "PATH")]
    ro_paths: Vec<PathBuf>,

    /// Bind path read-write at its real path inside the jail. Repeatable.
    #[arg(long = "rw", value_name = "PATH")]
    rw_paths: Vec<PathBuf>,

    /// Pass --dangerously-skip-permissions to claude.
    #[arg(long = "dangerous")]
    dangerous: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let home = PathBuf::from(env::var("HOME").context("HOME not set")?);
    let cwd = env::current_dir().context("cannot determine current directory")?;
    let bin_dir = PathBuf::from(
        env::var("CLAUDE_JAIL_BIN_DIR")
            .context("CLAUDE_JAIL_BIN_DIR not set — run via the nix package wrapper")?,
    );
    let bwrap_path = env::var("CLAUDE_JAIL_BWRAP").unwrap_or_else(|_| "bwrap".into());
    let nix_socket = env::var("AGENTIC_NIX_DAEMON")
        .unwrap_or_else(|_| "/nix/var/nix/daemon-socket/socket".to_string());

    // Pre-evaluate direnv on the host (trust is already established at the real path)
    let direnv_env = if cwd.join(".envrc").exists() {
        capture_direnv_env(&cwd).unwrap_or_default()
    } else {
        HashMap::new()
    };

    let mut b: Vec<OsString> = Vec::new();

    // Isolation: unshare everything except network (Claude needs internet)
    push(&mut b, &["--unshare-all", "--share-net"]);

    // Virtual filesystems
    push(&mut b, &["--proc", "/proc"]);
    push(&mut b, &["--dev", "/dev"]);
    push(&mut b, &["--tmpfs", "/tmp"]);

    // Nix store read-only
    push(&mut b, &["--ro-bind", "/nix", "/nix"]);

    // Nix daemon socket — bind if it exists
    if Path::new(&nix_socket).exists() {
        push(&mut b, &["--bind", &nix_socket, &nix_socket]);
    } else {
        eprintln!("warning: nix daemon socket not found at {nix_socket}");
        eprintln!("  set AGENTIC_NIX_DAEMON to override");
    }

    // Nix config and registry.
    // On NixOS: /etc/nix/registry.json -> /etc/static/nix/registry.json -> /nix/store/...
    // Both hops must be visible; /nix is already mounted, /etc/static is not.
    ro_bind_if_exists(&mut b, "/etc/static", "/etc/static");
    ro_bind_if_exists(&mut b, "/etc/nix", "/etc/nix");
    // Profiles needed for `nix profile` and PATH-from-profile patterns
    ro_bind_if_exists(&mut b, "/nix/var/nix/profiles", "/nix/var/nix/profiles");
    // Current NixOS system profile (for nix-env -qaP etc.)
    ro_bind_if_exists(&mut b, "/run/current-system", "/run/current-system");

    // SSL/TLS trust anchors.
    // NIX_SSL_CERT_FILE is set by the wrapper to pkgs.cacert so this always
    // resolves even if /etc/ssl is absent; the path lives under /nix which is
    // already mounted, so no extra bind is needed.
    for p in ["/etc/ssl", "/etc/ca-certificates", "/etc/pki/tls"] {
        ro_bind_if_exists(&mut b, p, p);
    }

    // DNS and name resolution
    for p in [
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/nsswitch.conf",
        "/etc/localtime",
        "/etc/passwd",
        "/etc/group",
    ] {
        ro_bind_if_exists(&mut b, p, p);
    }

    // Home directory as tmpfs; sub-mounts layered on top below
    b.push("--tmpfs".into());
    b.push(home.as_os_str().into());

    // ~/bin — pre-built tool symlinks
    bind(&mut b, "--ro-bind", &bin_dir, &home.join("bin"));

    // ~/.claude — read-write so Claude can persist conversation state
    let dot_claude = home.join(".claude");
    if !dot_claude.exists() {
        fs::create_dir_all(&dot_claude).context("creating ~/.claude")?;
    }
    bind(&mut b, "--bind", &dot_claude, &home.join(".claude"));

    // ~/.claude.json — Claude Code settings file
    let dot_claude_json = home.join(".claude.json");
    if dot_claude_json.exists() {
        bind(&mut b, "--bind", &dot_claude_json, &home.join(".claude.json"));
    }

    // ~/.gitconfig — read-only for git identity in commits
    ro_bind_if_exists(
        &mut b,
        &home.join(".gitconfig").to_string_lossy(),
        &home.join(".gitconfig").to_string_lossy(),
    );

    // ~/.config/git — git config dir (create parent dir first)
    let git_cfg_dir = home.join(".config").join("git");
    if git_cfg_dir.exists() {
        push(&mut b, &["--dir", &home.join(".config").to_string_lossy()]);
        bind(&mut b, "--ro-bind", &git_cfg_dir, &home.join(".config").join("git"));
    }

    // ~/.ssh/known_hosts — read-only for SSH host verification
    let known_hosts = home.join(".ssh").join("known_hosts");
    if known_hosts.exists() {
        push(&mut b, &["--dir", &home.join(".ssh").to_string_lossy()]);
        bind(&mut b, "--ro-bind", &known_hosts, &home.join(".ssh").join("known_hosts"));
    }

    // SSH agent socket — forward for git-over-SSH auth
    if let Ok(sock) = env::var("SSH_AUTH_SOCK") {
        let sock_path = PathBuf::from(&sock);
        if sock_path.exists() {
            bind(&mut b, "--bind", &sock_path, &sock_path);
        }
    }

    // Bind the current directory at its real path so $PWD, realpath("."),
    // direnv trust hashes, and any tool keyed on the absolute path all match.
    // --dir uses mkdir -p semantics in the sandbox namespace.
    if let Some(parent) = cwd.parent() {
        push(&mut b, &["--dir", &parent.to_string_lossy()]);
    }
    bind(&mut b, "--bind", &cwd, &cwd);

    // Extra read-only paths — bind at real path (mkdir -p parent first)
    for path in &args.ro_paths {
        if let Some(parent) = path.parent() {
            push(&mut b, &["--dir", &parent.to_string_lossy()]);
        }
        bind(&mut b, "--ro-bind", path, path);
    }

    // Extra read-write paths — bind at real path (mkdir -p parent first)
    for path in &args.rw_paths {
        if let Some(parent) = path.parent() {
            push(&mut b, &["--dir", &parent.to_string_lossy()]);
        }
        bind(&mut b, "--bind", path, path);
    }

    // Clear inherited environment; set everything explicitly below.
    b.push("--clearenv".into());

    // Core
    setenv(&mut b, "HOME", &home.to_string_lossy());

    let jail_path = match direnv_env.get("PATH") {
        // Prepend ~/bin to whatever nix develop / direnv added
        Some(dp) => format!("{}/bin:{dp}", home.display()),
        None => format!("{}/bin", home.display()),
    };
    setenv(&mut b, "PATH", &jail_path);

    setenv(&mut b, "USER", &env::var("USER").unwrap_or_else(|_| "user".into()));
    passthrough(&mut b, "LOGNAME");

    // Nix daemon
    setenv(&mut b, "NIX_REMOTE", "daemon");
    setenv(&mut b, "NIX_DAEMON_SOCKET_PATH", &nix_socket);
    passthrough(&mut b, "NIX_PATH");
    // Merge experimental features: honour any host NIX_CONFIG then append ours
    let host_nix_config = env::var("NIX_CONFIG").unwrap_or_default();
    let nix_config = if host_nix_config.is_empty() {
        "extra-experimental-features = nix-command flakes".into()
    } else {
        format!("{host_nix_config}\nextra-experimental-features = nix-command flakes")
    };
    setenv(&mut b, "NIX_CONFIG", &nix_config);

    // TLS — prefer NIX_SSL_CERT_FILE from host, fall back to well-known paths
    if let Ok(cert) = env::var("NIX_SSL_CERT_FILE") {
        setenv(&mut b, "NIX_SSL_CERT_FILE", &cert);
        setenv(&mut b, "SSL_CERT_FILE", &cert);
    } else {
        for cert_path in [
            "/etc/ssl/certs/ca-bundle.crt",
            "/etc/ssl/certs/ca-certificates.crt",
        ] {
            if Path::new(cert_path).exists() {
                setenv(&mut b, "NIX_SSL_CERT_FILE", cert_path);
                setenv(&mut b, "SSL_CERT_FILE", cert_path);
                break;
            }
        }
    }

    // Terminal
    for var in ["TERM", "COLORTERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION"] {
        passthrough(&mut b, var);
    }

    // Locale
    for var in ["LANG", "LC_ALL", "LC_CTYPE", "LC_MESSAGES", "LC_COLLATE", "LC_TIME"] {
        passthrough(&mut b, var);
    }

    // Proxy settings
    for var in [
        "HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY",
        "http_proxy", "https_proxy", "no_proxy",
    ] {
        passthrough(&mut b, var);
    }

    // Anthropic / Claude auth
    for var in ["ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN"] {
        passthrough(&mut b, var);
    }
    for (k, v) in env::vars() {
        if k.starts_with("CLAUDE_") {
            setenv(&mut b, &k, &v);
        }
    }

    // SSH agent
    if let Ok(v) = env::var("SSH_AUTH_SOCK") {
        setenv(&mut b, "SSH_AUTH_SOCK", &v);
    }

    // Git identity overrides (useful in CI / personal configs)
    for var in [
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
    ] {
        passthrough(&mut b, var);
    }

    // direnv-provided variables (PATH already merged above)
    for (k, v) in &direnv_env {
        if k == "PATH" {
            continue;
        }
        setenv(&mut b, k, v);
    }

    // TMPDIR inside jail
    setenv(&mut b, "TMPDIR", "/tmp");

    // Change to the real path so $PWD matches what the user expects
    b.push("--chdir".into());
    b.push(cwd.as_os_str().into());

    // Command to exec
    b.push("--".into());
    b.push("claude".into());
    if args.dangerous {
        b.push("--dangerously-skip-permissions".into());
    }

    let err = Command::new(&bwrap_path).args(&b).exec();
    Err(anyhow::anyhow!("exec {bwrap_path}: {err}"))
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn push(b: &mut Vec<OsString>, args: &[&str]) {
    for a in args {
        b.push((*a).into());
    }
}

fn bind(b: &mut Vec<OsString>, flag: &str, src: &Path, dst: &Path) {
    b.push(flag.into());
    b.push(src.as_os_str().into());
    b.push(dst.as_os_str().into());
}

fn ro_bind_if_exists(b: &mut Vec<OsString>, src: &str, dst: &str) {
    if Path::new(src).exists() {
        b.push("--ro-bind".into());
        b.push(src.into());
        b.push(dst.into());
    }
}

fn setenv(b: &mut Vec<OsString>, key: &str, val: &str) {
    b.push("--setenv".into());
    b.push(key.into());
    b.push(val.into());
}

fn passthrough(b: &mut Vec<OsString>, key: &str) {
    if let Ok(val) = env::var(key) {
        setenv(b, key, &val);
    }
}

fn capture_direnv_env(dir: &Path) -> Result<HashMap<String, String>> {
    let out = Command::new("direnv")
        .args(["export", "json"])
        .current_dir(dir)
        .output()?;

    if !out.status.success() || out.stdout.is_empty() {
        return Ok(HashMap::new());
    }

    // direnv export json: {"KEY": "value"} or {"KEY": null} for unsets
    let raw: HashMap<String, Option<String>> = serde_json::from_slice(&out.stdout)?;
    Ok(raw.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect())
}
