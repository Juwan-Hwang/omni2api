//! `omni2api up` — the zero-setup mode.
//!
//! One command, no database, no server to rent, no account:
//!
//! 1. an in-memory store with a caller key and a responder (seat) key,
//!    minted once and kept in `~/.omni2api/keys.json` so restarts keep them;
//! 2. the API listening on localhost;
//! 3. a Cloudflare quick tunnel (cloudflared is downloaded on first use)
//!    giving a public `https://….trycloudflare.com` address;
//! 4. everything a caller or a seat needs printed and written to
//!    `~/.omni2api/endpoint.json`.
//!
//! Callers resend the whole conversation on every request, so nothing has
//! to survive a restart except the keys.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::model::identity::{
    AgentIdentity, ApiKey, ClientType, HumanIdentity, HumanStatus, KeyScope,
};
use crate::storage::memory::MemoryStore;
use crate::storage::traits::IdentityRepository;
use crate::storage::AppState;

/// Model name served until the user pins one (`--model`) or the seat
/// chooses its own (`oh-seat name …`). Deliberately neutral: the name
/// belongs to whoever sits in the seat, not to this software.
pub const DEFAULT_MODEL: &str = "omni2api";

/// Version of the JSON that `up --json`, `status --json` and endpoint.json
/// carry. The contract (SETUP.md, "Machine-readable status"): fields are
/// only ever added; none is renamed, retyped or removed without bumping it.
pub const SCHEMA: u64 = 1;

/// `--json` was given: print one JSON object instead of the banner.
static JSON_OUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn json_out() -> bool {
    JSON_OUT.load(std::sync::atomic::Ordering::Relaxed)
}

/// The status object: endpoint.json plus whether `up` is alive.
pub fn status_object(dir: &Path) -> serde_json::Value {
    let pid = read_pid(dir).filter(|p| pid_alive(*p));
    let info = std::fs::read_to_string(dir.join("endpoint.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    match (pid, info) {
        (Some(pid), Some(mut info)) => {
            info["running"] = serde_json::json!(true);
            info["pid"] = serde_json::json!(pid);
            info["schema"] = serde_json::json!(SCHEMA);
            info
        }
        (pid, _) => serde_json::json!({
            "schema": SCHEMA,
            "running": false,
            "pid": pid,
            "error": if pid.is_some() { "starting: endpoint not published yet" } else { "not running" },
        }),
    }
}

fn print_json(v: &serde_json::Value) {
    println!("{}", serde_json::to_string(v).unwrap_or_default());
}

pub const USAGE: &str = "\
omni2api — put anything (an AI agent, a script, you) behind an OpenAI-compatible API

  omni2api up -d     start in the background; prints local + public base_url, keys, model
  omni2api up        same; stays in the foreground only in a real terminal
                     (--foreground forces it, e.g. under systemd or Docker)
  omni2api status    show the running endpoint (exit 0 running, 1 not)
  omni2api down      stop the background one
  --json             on up/status/down: one JSON object on stdout instead of text
  omni2api --version print the version

  up options: --port N (8080)  --host H (127.0.0.1)  --no-tunnel  --tunnel auto|cloudflare|pinggy|token|none  --name \"Seat name\"  --model \"Model name\"
  named tunnel (fixed domain): OMNI2API_TUNNEL_TOKEN=... OMNI2API_PUBLIC_URL=https://api.example.com
  state: ~/.omni2api (keys.json, endpoint.json, up.log); OMNI2API_HOME overrides

Then answer requests with the seat CLI: oh-seat guide
Advanced: `omni2api serve` runs the full server (OMNI2API_BACKEND=postgres for a database).
";

/// Options for `omni2api up`, from flags and environment.
#[derive(Debug, Clone)]
pub struct UpOptions {
    pub port: u16,
    pub host: String,
    /// `auto` (default: cloudflare, then pinggy), one of those, `none`,
    /// or `token` (named Cloudflare tunnel, needs `OMNI2API_TUNNEL_TOKEN`
    /// and `OMNI2API_PUBLIC_URL`).
    pub tunnel: String,
    pub name: String,
    /// Model name pinned by the user (`--model` / `OMNI2API_MODEL`).
    /// Beats the seat's own choice; see `storage::ModelName`.
    pub model: Option<String>,
}

impl UpOptions {
    pub fn from_args(args: &[String]) -> Self {
        let get = |flag: &str| -> Option<String> {
            args.iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(i + 1).cloned())
        };
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let tunnel = if args.iter().any(|a| a == "--no-tunnel") {
            "none".to_string()
        } else if let Some(t) = get("--tunnel").or_else(|| env("OMNI2API_TUNNEL")) {
            t
        } else if env("OMNI2API_TUNNEL_TOKEN").is_some() {
            "token".to_string()
        } else {
            "auto".to_string()
        };
        Self {
            port: get("--port")
                .or_else(|| env("OMNI2API_PORT"))
                .and_then(|p| p.parse().ok())
                .unwrap_or(8080),
            host: get("--host")
                .or_else(|| env("OMNI2API_HOST"))
                .unwrap_or_else(|| "127.0.0.1".into()),
            tunnel,
            name: get("--name")
                .or_else(|| env("OMNI2API_NAME"))
                .unwrap_or_else(|| "Omni2API seat".into()),
            model: get("--model")
                .or_else(|| env("OMNI2API_MODEL"))
                .filter(|m| crate::storage::is_valid_model_name(m.trim()))
                .map(|m| m.trim().to_string()),
        }
    }
}

/// `~/.omni2api` (or `OMNI2API_HOME`).
pub fn home_dir() -> PathBuf {
    if let Ok(h) = std::env::var("OMNI2API_HOME") {
        if !h.trim().is_empty() {
            return PathBuf::from(h.trim());
        }
    }
    let base = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    Path::new(&base).join(".omni2api")
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Keys {
    pub caller_key: String,
    pub responder_key: String,
}

fn mint() -> String {
    format!(
        "ohk_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Keys from the environment, else `keys.json`, else freshly minted (and saved).
pub fn load_or_mint_keys(dir: &Path) -> anyhow::Result<Keys> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let path = dir.join("keys.json");
    let saved: Option<Keys> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let keys = Keys {
        caller_key: env("OMNI2API_CALLER_KEY")
            .or_else(|| saved.as_ref().map(|k| k.caller_key.clone()))
            .unwrap_or_else(mint),
        responder_key: env("OMNI2API_RESPONDER_KEY")
            .or_else(|| saved.as_ref().map(|k| k.responder_key.clone()))
            .unwrap_or_else(mint),
    };
    if saved.as_ref().map(|s| (&s.caller_key, &s.responder_key))
        != Some((&keys.caller_key, &keys.responder_key))
    {
        std::fs::create_dir_all(dir)?;
        write_private(&path, &serde_json::to_string_pretty(&keys)?)?;
    }
    Ok(keys)
}

fn write_private(path: &Path, body: &str) -> anyhow::Result<()> {
    std::fs::write(path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// In-memory state holding one human seat, a caller key and a responder key.
/// `model` is the user-pinned model name (`--model`); it beats both the
/// environment default and any name the seat later chooses for itself.
pub async fn build_state(keys: &Keys, name: &str, model: Option<&str>) -> AppState {
    let store = MemoryStore::new();
    let now = chrono::Utc::now();
    let human_id = "hum_local".to_string();
    store
        .insert_human(HumanIdentity {
            id: human_id.clone(),
            display_name: name.to_string(),
            avatar_url: None,
            status: Some(HumanStatus::Online),
            capabilities: Vec::new(),
            metadata: HashMap::new(),
            created_at: now,
            updated_at: now,
        })
        .await;
    for (agent, key_id, raw, scope) in [
        (
            "agt_caller",
            "key_caller",
            &keys.caller_key,
            KeyScope::Caller,
        ),
        (
            "agt_seat",
            "key_seat",
            &keys.responder_key,
            KeyScope::Responder,
        ),
    ] {
        store
            .insert_agent(AgentIdentity {
                id: agent.to_string(),
                name: agent.to_string(),
                client_type: ClientType::Custom,
                api_key_id: key_id.to_string(),
                capabilities: Vec::new(),
                metadata: HashMap::new(),
                created_at: now,
                updated_at: now,
            })
            .await;
        store
            .insert_api_key(ApiKey::new(key_id, raw, agent, &human_id).with_scope(scope))
            .await;
    }
    let state = AppState::from_store(store);
    if let Some(m) = model {
        state.model.set_user(m);
    }
    state
}

/// The cloudflared asset for this platform on its GitHub releases page.
fn cloudflared_asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("cloudflared-linux-amd64"),
        ("linux", "aarch64") => Some("cloudflared-linux-arm64"),
        ("linux", "arm") => Some("cloudflared-linux-arm"),
        ("windows", "x86_64") => Some("cloudflared-windows-amd64.exe"),
        ("windows", "x86") => Some("cloudflared-windows-386.exe"),
        ("macos", "aarch64") => Some("cloudflared-darwin-arm64.tgz"),
        ("macos", "x86_64") => Some("cloudflared-darwin-amd64.tgz"),
        _ => None,
    }
}

fn on_path(bin: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{bin}.exe")
    } else {
        bin.to_string()
    };
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join(&exe))
            .find(|c| c.is_file())
    })
}

/// cloudflared from `OMNI2API_CLOUDFLARED`, `PATH`, `~/.omni2api/bin`, or
/// downloaded there from Cloudflare's GitHub releases.
pub async fn ensure_cloudflared(dir: &Path) -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("OMNI2API_CLOUDFLARED") {
        if !p.trim().is_empty() {
            return Ok(PathBuf::from(p.trim()));
        }
    }
    if let Some(p) = on_path("cloudflared") {
        return Ok(p);
    }
    let bin_dir = dir.join("bin");
    let local = bin_dir.join(if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    });
    if local.is_file() {
        return Ok(local);
    }
    let asset = cloudflared_asset().ok_or_else(|| {
        anyhow::anyhow!(
            "no cloudflared build for {}/{}; install cloudflared yourself or run with --no-tunnel",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let url = format!("https://github.com/cloudflare/cloudflared/releases/latest/download/{asset}");
    eprintln!("omni2api: downloading cloudflared ({asset})…");
    let fetched = async {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await
    }
    .await;
    let bytes = fetched.map_err(|e| {
        anyhow::anyhow!(
            "download failed ({e}). Get {url} another way (browser, proxy) and save it as {}",
            local.display()
        )
    })?;
    std::fs::create_dir_all(&bin_dir)?;
    if asset.ends_with(".tgz") {
        let tgz = bin_dir.join(asset);
        std::fs::write(&tgz, &bytes)?;
        let ok = Command::new("tar")
            .arg("-xzf")
            .arg(&tgz)
            .arg("-C")
            .arg(&bin_dir)
            .status()?
            .success();
        let _ = std::fs::remove_file(&tgz);
        if !ok || !local.is_file() {
            anyhow::bail!("could not unpack {asset}");
        }
    } else {
        std::fs::write(&local, &bytes)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(local)
}

/// Pull the first `https://<x>.trycloudflare.com` out of a log line.
pub fn find_quick_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '|' || c == '"')
        .unwrap_or(rest.len());
    let url = &rest[..end];
    url.ends_with(".trycloudflare.com").then(|| url.to_string())
}

/// Start the tunnel. Returns the child and the public URL (when known).
pub fn start_tunnel(
    cloudflared: &Path,
    opts: &UpOptions,
    local_url: &str,
) -> anyhow::Result<(Child, Option<String>)> {
    if opts.tunnel == "token" {
        let token = std::env::var("OMNI2API_TUNNEL_TOKEN")
            .map_err(|_| anyhow::anyhow!("--tunnel token needs OMNI2API_TUNNEL_TOKEN"))?;
        let child = Command::new(cloudflared)
            .args(["tunnel", "--no-autoupdate", "run", "--token", token.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let public = std::env::var("OMNI2API_PUBLIC_URL").ok();
        return Ok((child, public));
    }
    let mut child = Command::new(cloudflared)
        .args(["tunnel", "--no-autoupdate", "--url", local_url])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = child.stderr.take().expect("piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut sent = false;
        let mut log = tunnel_log("cloudflare");
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Some(f) = log.as_mut() {
                use std::io::Write;
                let _ = writeln!(f, "{line}");
            }
            if !sent {
                if let Some(u) = find_quick_url(&line) {
                    let _ = tx.send(u);
                    sent = true;
                }
            }
            // keep draining so cloudflared never blocks on a full pipe
        }
    });
    let url = rx.recv_timeout(Duration::from_secs(60)).ok();
    Ok((child, url))
}

/// A fresh quick-tunnel hostname takes a few seconds to resolve and route.
/// Hand out the URL only once a request through it gets an answer.
async fn wait_reachable(public: &str, caller_key: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    else {
        return false;
    };
    let url = format!("{}/v1/models", public.trim_end_matches('/'));
    // Bounded: on a filtered network every provider would otherwise burn
    // minutes before `up` gives its answer.
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    while std::time::Instant::now() < deadline {
        if let Ok(r) = client.get(&url).bearer_auth(caller_key).send().await {
            if r.status().is_success() {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    false
}

/// Providers tried by `--tunnel auto`, in order.
const AUTO: [&str; 2] = ["cloudflare", "pinggy"];

/// Open a public tunnel. Returns (children to kill, url, provider, verified).
/// `auto` tries each provider; a URL that answers a request wins, otherwise
/// the first URL obtained is kept unverified (this machine's own egress may
/// be what blocks the check).
async fn open_tunnel(
    opts: &UpOptions,
    dir: &Path,
    local: &str,
    caller_key: &str,
) -> (Vec<Child>, Option<String>, Option<String>, bool) {
    let list: Vec<&str> = match opts.tunnel.as_str() {
        "none" => return (Vec::new(), None, None, false),
        "auto" => AUTO.to_vec(),
        "quick" | "cloudflared" => vec!["cloudflare"],
        other => vec![other],
    };
    let mut children: Vec<(String, Child)> = Vec::new();
    let mut fallback: Option<(String, String)> = None;
    for provider in list {
        let started = match provider {
            "cloudflare" | "token" => match ensure_cloudflared(dir).await {
                Ok(cf) => start_tunnel(&cf, opts, local),
                Err(e) => Err(e),
            },
            "pinggy" => start_ssh_tunnel(provider, local),
            other => Err(anyhow::anyhow!(
                "unknown tunnel {other:?} (auto, cloudflare, pinggy, token, none)"
            )),
        };
        match started {
            Ok((child, Some(url))) => {
                children.push((provider.to_string(), child));
                if wait_reachable(&url, caller_key).await {
                    // Only the tunnel we hand out stays open: an unreported
                    // public entry to this machine is not acceptable.
                    let kept = keep_only(children, provider);
                    return (kept, Some(url), Some(provider.to_string()), true);
                }
                eprintln!("omni2api: {provider}: {url} does not answer from this machine");
                fallback.get_or_insert((url, provider.to_string()));
            }
            Ok((mut child, None)) => {
                eprintln!("omni2api: {provider}: no public URL");
                if provider == "token" {
                    children.push((provider.to_string(), child));
                } else {
                    let _ = child.kill();
                }
            }
            Err(e) => eprintln!("omni2api: {provider}: {e}"),
        }
    }
    match fallback {
        Some((url, p)) => (keep_only(children, &p), Some(url), Some(p), false),
        None => (keep_only(children, "token"), None, None, false),
    }
}

fn keep_only(children: Vec<(String, Child)>, provider: &str) -> Vec<Child> {
    children
        .into_iter()
        .filter_map(|(p, mut c)| {
            if p == provider {
                Some(c)
            } else {
                let _ = c.kill();
                None
            }
        })
        .collect()
}

/// Each tunnel's own output goes to `~/.omni2api/tunnel-<provider>.log`,
/// so a tunnel that dies later can be diagnosed.
fn tunnel_log(provider: &str) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home_dir().join(format!("tunnel-{provider}.log")))
        .ok()
}

/// pinggy (port 443) through the system `ssh`; no account, no binary to
/// download.
fn start_ssh_tunnel(provider: &str, local: &str) -> anyhow::Result<(Child, Option<String>)> {
    let ssh = on_path("ssh").ok_or_else(|| anyhow::anyhow!("no ssh client on PATH"))?;
    let port = local.rsplit(':').next().unwrap_or("8080");
    let mut cmd = Command::new(ssh);
    cmd.args([
        "-T",
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "ServerAliveInterval=30",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ConnectTimeout=15",
    ]);
    #[cfg(unix)]
    cmd.args(["-o", "UserKnownHostsFile=/dev/null"]);
    #[cfg(windows)]
    cmd.args(["-o", "UserKnownHostsFile=NUL"]);
    cmd.args([
        "-p",
        "443",
        "-R",
        &format!("0:127.0.0.1:{port}"),
        "a.pinggy.io",
    ]);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        let name = provider.to_string();
        std::thread::spawn(move || {
            let mut log = tunnel_log(&name);
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if let Some(f) = log.as_mut() {
                    use std::io::Write;
                    let _ = writeln!(f, "{line}");
                }
                if let Some(u) = find_ssh_url(&line) {
                    let _ = tx.send(u);
                }
            }
        });
    }
    let url = rx.recv_timeout(Duration::from_secs(30)).ok();
    Ok((child, url))
}

/// The public https URL in pinggy output.
pub fn find_ssh_url(line: &str) -> Option<String> {
    line.split(|c: char| c.is_whitespace() || c == ',')
        .map(|w| w.trim_matches(|c: char| c == '"' || c == '\'' || c == '|'))
        .filter(|w| w.starts_with("https://"))
        .find(|w| {
            let host = w
                .trim_start_matches("https://")
                .split('/')
                .next()
                .unwrap_or("");
            host.contains("pinggy")
                && !host.starts_with("dashboard.")
                && !host.starts_with("admin.")
        })
        .map(|w| w.trim_end_matches('/').to_string())
}

/// What `endpoint.json` holds and `up` prints.
pub fn endpoint_json(
    local: &str,
    public: Option<&str>,
    keys: &Keys,
    model: &str,
) -> serde_json::Value {
    let base = |u: &str| format!("{}/v1", u.trim_end_matches('/'));
    serde_json::json!({
        "schema": SCHEMA,
        "model": model,
        "local_base_url": base(local),
        "public_base_url": public.map(base),
        "caller_key": keys.caller_key,
        "responder_key": keys.responder_key,
        "seat": {
            "OH_SEAT_URL": local,
            "OH_SEAT_KEY": keys.responder_key,
        },
    })
}

pub fn print_banner(info: &serde_json::Value) {
    let s = |k: &str| info[k].as_str().unwrap_or("-").to_string();
    println!();
    println!("Omni2API is up.");
    println!();
    println!("  Caller config (OpenAI-compatible, or Anthropic at the same base):");
    println!("    base_url  (local)  {}", s("local_base_url"));
    println!("    base_url  (public) {}", s("public_base_url"));
    println!("    api_key            {}", s("caller_key"));
    println!("    model              {}", s("model"));
    if let Some(note) = info["public_note"].as_str() {
        println!("    note: {note}");
    }
    println!();
    println!("  Seat (the thing that answers) — run in another terminal:");
    println!(
        "    OH_SEAT_URL={} OH_SEAT_KEY={} oh-seat next",
        info["seat"]["OH_SEAT_URL"].as_str().unwrap_or("-"),
        info["seat"]["OH_SEAT_KEY"].as_str().unwrap_or("-")
    );
    println!();
    println!("  Saved to {}", home_dir().join("endpoint.json").display());
    println!();
}

fn is_detach(a: &str) -> bool {
    a == "-d" || a == "--detach" || a == "--background"
}

/// `up -d`: start `up` as a background process that outlives this shell,
/// wait for its endpoint, print it, and return. Built for agents whose
/// shell tool cannot hold a long-running foreground process.
fn detach(args: &[String], dir: &Path) -> anyhow::Result<()> {
    let endpoint = dir.join("endpoint.json");
    let _ = std::fs::remove_file(&endpoint);
    if let Some(pid) = read_pid(dir) {
        if pid_alive(pid) {
            kill_pid(pid);
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    let log = std::fs::File::create(dir.join("up.log"))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("up")
        .args(args.iter().filter(|a| !is_detach(a)))
        .arg("--foreground")
        .env("OMNI2API_CHILD", "1")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
        cmd.creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000);
    }
    let child = cmd.spawn()?;
    std::fs::write(dir.join("up.pid"), child.id().to_string())?;
    for _ in 0..360 {
        std::thread::sleep(Duration::from_millis(500));
        if let Some(info) = std::fs::read_to_string(&endpoint)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        {
            if json_out() {
                print_json(&status_object(dir));
                return Ok(());
            }
            print_banner(&info);
            println!(
                "  Running in the background (pid {}). Log: {}",
                child.id(),
                dir.join("up.log").display()
            );
            println!("  Stop: omni2api down    Status: omni2api status");
            println!();
            return Ok(());
        }
        if !pid_alive(child.id()) {
            break;
        }
    }
    let log = std::fs::read_to_string(dir.join("up.log")).unwrap_or_default();
    if json_out() {
        print_json(&serde_json::json!({
            "schema": SCHEMA,
            "running": false,
            "pid": null,
            "error": "did not come up",
            "log": log,
        }));
        std::process::exit(1);
    }
    anyhow::bail!("omni2api did not come up. Log:\n{log}")
}

fn read_pid(dir: &Path) -> Option<u32> {
    std::fs::read_to_string(dir.join("up.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
}

fn kill_pid(pid: u32) {
    #[cfg(unix)]
    let _ = Command::new("kill").arg(pid.to_string()).status();
    #[cfg(windows)]
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .status();
}

/// `omni2api status [--json]`: is it running, and where.
/// Exit 0 running, 1 not running (also with --json).
pub fn run_status(args: &[String]) -> anyhow::Result<()> {
    let st = status_object(&home_dir());
    let running = st["running"].as_bool() == Some(true);
    if args.iter().any(|a| a == "--json") {
        print_json(&st);
    } else if running {
        print_banner(&st);
        println!("  Running (pid {}).", st["pid"]);
    } else {
        println!("omni2api is not running in the background. Start it: omni2api up -d");
    }
    if !running {
        std::process::exit(1)
    }
    Ok(())
}

/// `omni2api down`: stop the background `up`.
/// `omni2api down [--json]`: exit 0 either way (it is not running after).
pub fn run_down(args: &[String]) -> anyhow::Result<()> {
    let dir = home_dir();
    let json = args.iter().any(|a| a == "--json");
    let pid = read_pid(&dir).filter(|p| pid_alive(*p));
    if let Some(pid) = pid {
        kill_pid(pid);
    }
    let _ = std::fs::remove_file(dir.join("up.pid"));
    let _ = std::fs::remove_file(dir.join("endpoint.json"));
    if json {
        print_json(&serde_json::json!({
            "schema": SCHEMA,
            "running": false,
            "stopped_pid": pid,
        }));
    } else {
        match pid {
            Some(pid) => println!("stopped (pid {pid})"),
            None => println!("omni2api was not running"),
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub async fn run_up(args: &[String]) -> anyhow::Result<()> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return Ok(());
    }
    // Validate instead of grazing for the flags we know and ignoring the
    // rest: `up --help` used to start a second instance, and a typo'd flag
    // silently became a misconfigured one. Value flags consume the next
    // argument, exactly as UpOptions::from_args reads them.
    const VALUE_FLAGS: [&str; 5] = ["--tunnel", "--port", "--host", "--name", "--model"];
    const SWITCH_FLAGS: [&str; 7] = [
        "--no-tunnel",
        "--json",
        "--foreground",
        "-f",
        "-d",
        "--detach",
        "--background",
    ];
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if VALUE_FLAGS.contains(&a.as_str()) {
            i += 2;
        } else if SWITCH_FLAGS.contains(&a.as_str()) {
            i += 1;
        } else {
            eprintln!("omni2api up: unknown argument '{a}' — nothing was started.\n\n{USAGE}");
            std::process::exit(2);
        }
    }
    if args.iter().any(|a| a == "--json") {
        JSON_OUT.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let opts = UpOptions::from_args(args);
    let dir = home_dir();
    std::fs::create_dir_all(&dir)?;
    // An agent's shell tool has no terminal and blocks until the command
    // exits, so a foreground `up` would hang it: detach unless asked not to.
    use std::io::IsTerminal;
    let foreground = args.iter().any(|a| a == "--foreground" || a == "-f");
    let no_tty = !std::io::stdout().is_terminal();
    if (args.iter().any(|a| is_detach(a)) || (no_tty && !foreground))
        && std::env::var("OMNI2API_CHILD").is_err()
    {
        return detach(args, &dir);
    }
    // Request dumps (observation.rs) are a debugging aid for the hosted
    // server; here they would litter whatever directory `up` started in.
    if std::env::var_os("OMNI2API_OBSERVATION").is_none() {
        std::env::set_var("OMNI2API_OBSERVATION", "off");
    }
    let keys = load_or_mint_keys(&dir)?;
    let state = build_state(&keys, &opts.name, opts.model.as_deref()).await;
    let model = state.model.clone();
    let app = crate::build_router(state);

    let listener = match tokio::net::TcpListener::bind((opts.host.as_str(), opts.port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "omni2api: port {} busy ({e}); picking a free one",
                opts.port
            );
            tokio::net::TcpListener::bind((opts.host.as_str(), 0)).await?
        }
    };
    let port = listener.local_addr()?.port();
    let local = format!("http://127.0.0.1:{port}");
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let (tunnel_children, public, provider, verified) =
        open_tunnel(&opts, &dir, &local, &keys.caller_key).await;
    publish_endpoint(
        &dir,
        &local,
        &keys,
        &opts,
        &PublishedTunnel {
            public: public.as_deref(),
            provider: provider.as_deref(),
            verified,
        },
        &model,
    )?;
    let children = std::sync::Arc::new(std::sync::Mutex::new(tunnel_children));
    // Free tunnels die silently (pinggy after 60 minutes, others whenever
    // the provider feels like it). Watch the tunnel we handed out and open
    // a new one when it dies; endpoint.json always holds the current URL.
    // This must also run when the URL could not be verified from here: a
    // network whose egress blocks the self-check is exactly the network
    // that needs a watcher. There, probing is meaningless, so watch the
    // tunnel processes themselves instead.
    if public.is_some() && matches!(provider.as_deref(), Some("cloudflare" | "pinggy")) {
        let (children, dir, local, keys, opts, model) = (
            children.clone(),
            dir.clone(),
            local.clone(),
            keys.clone(),
            opts.clone(),
            model.clone(),
        );
        let mut current = public.clone().unwrap_or_default();
        tokio::spawn(async move {
            let mut misses = 0;
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let dead = if verified {
                    if probe_once(&current, &keys.caller_key).await {
                        misses = 0;
                        false
                    } else {
                        misses += 1;
                        misses >= 3
                    }
                } else {
                    children_all_dead(&children)
                };
                if !dead {
                    continue;
                }
                misses = 0;
                eprintln!("omni2api: tunnel for {current} died; opening a new one");
                for mut c in children
                    .lock()
                    .map(|mut v| std::mem::take(&mut *v))
                    .unwrap_or_default()
                {
                    let _ = c.kill();
                }
                let (new_children, public, provider, verified) =
                    open_tunnel(&opts, &dir, &local, &keys.caller_key).await;
                if let Ok(mut v) = children.lock() {
                    *v = new_children;
                }
                let _ = publish_endpoint(
                    &dir,
                    &local,
                    &keys,
                    &opts,
                    &PublishedTunnel {
                        public: public.as_deref(),
                        provider: provider.as_deref(),
                        verified,
                    },
                    &model,
                );
                if let Some(p) = public {
                    eprintln!("omni2api: new public URL {p}");
                    current = p;
                }
            }
        });
    }

    let res = tokio::select! {
        r = server => r.map_err(anyhow::Error::from).and_then(|r| r.map_err(anyhow::Error::from)),
        _ = shutdown_signal() => Ok(()),
    };
    for mut c in children
        .lock()
        .map(|mut v| std::mem::take(&mut *v))
        .unwrap_or_default()
    {
        let _ = c.kill();
    }
    res
}

/// True when every tunnel child process has exited. An empty set is not
/// "dead" — it means there is no tunnel process to watch.
fn children_all_dead(children: &std::sync::Mutex<Vec<Child>>) -> bool {
    let Ok(mut v) = children.lock() else {
        return false;
    };
    !v.is_empty()
        && v.iter_mut()
            .all(|c| matches!(c.try_wait(), Ok(Some(_)) | Err(_)))
}

async fn probe_once(public: &str, caller_key: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
    else {
        return true;
    };
    let url = format!("{}/v1/models", public.trim_end_matches('/'));
    matches!(client.get(&url).bearer_auth(caller_key).send().await, Ok(r) if r.status().is_success())
}

/// The tunnel facts `endpoint.json` reports alongside the addresses.
struct PublishedTunnel<'a> {
    public: Option<&'a str>,
    provider: Option<&'a str>,
    verified: bool,
}

fn publish_endpoint(
    dir: &std::path::Path,
    local: &str,
    keys: &Keys,
    opts: &UpOptions,
    tunnel: &PublishedTunnel<'_>,
    model: &crate::storage::ModelName,
) -> anyhow::Result<()> {
    let (public, provider, verified) = (tunnel.public, tunnel.provider, tunnel.verified);
    let mut info = endpoint_json(local, public, keys, &model.get());
    info["tunnel"] = serde_json::json!(provider);
    if provider == Some("pinggy") {
        info["public_note"] = serde_json::json!(
            "pinggy free tunnels expire after 60 minutes; omni2api opens a new one and updates this file"
        );
    }
    if public.is_some() && !verified {
        info["public_note"] = serde_json::json!(
            "this machine could not reach its own public URL (an outbound filter on this network?); it may still work from elsewhere: test it from another network"
        );
    } else if public.is_none() && opts.tunnel != "none" {
        info["public_note"] = serde_json::json!(
            "no tunnel could be opened from this machine (outbound traffic to Cloudflare and pinggy blocked?). The local URL works; for a public one run omni2api on a machine with normal internet access, or use a named tunnel / your own server"
        );
    }
    info["tunnel_verified"] = serde_json::json!(public.is_some() && verified);
    write_private(
        &dir.join("endpoint.json"),
        &serde_json::to_string_pretty(&info)?,
    )?;
    // A detached child's stdout is up.log; the parent prints the result.
    if json_out() {
        if std::env::var("OMNI2API_CHILD").is_err() {
            let mut v = info.clone();
            v["running"] = serde_json::json!(true);
            v["pid"] = serde_json::json!(std::process::id());
            print_json(&v);
        }
    } else {
        print_banner(&info);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_object_reports_not_running_with_schema() {
        let dir = std::env::temp_dir().join(format!("o2a-status-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let st = status_object(&dir);
        assert_eq!(st["schema"], SCHEMA);
        assert_eq!(st["running"], false);
        assert!(st["pid"].is_null());
        assert_eq!(st["error"], "not running");
        let keys = Keys {
            caller_key: "c".into(),
            responder_key: "r".into(),
        };
        let ep = endpoint_json("http://127.0.0.1:1", None, &keys, "m");
        assert_eq!(ep["schema"], SCHEMA);
        assert!(ep["public_base_url"].is_null());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_quick_url_in_cloudflared_box() {
        let line = "2026-10-22T03:50:00Z INF |  https://brave-otter-lake.trycloudflare.com                        |";
        assert_eq!(
            find_quick_url(line).as_deref(),
            Some("https://brave-otter-lake.trycloudflare.com")
        );
        assert_eq!(
            find_quick_url("INF see https://www.cloudflare.com/website-terms/"),
            None
        );
    }

    #[test]
    fn finds_ssh_tunnel_urls() {
        assert_eq!(
            find_ssh_url("https://acifd-129-225-183-175.free.pinggy.net\r").as_deref(),
            Some("https://acifd-129-225-183-175.free.pinggy.net")
        );
        assert_eq!(
            find_ssh_url("https://hcdxe-1-2-3-4.run.pinggy-free.link").as_deref(),
            Some("https://hcdxe-1-2-3-4.run.pinggy-free.link")
        );
        assert_eq!(
            find_ssh_url("visit https://dashboard.pinggy.io for more"),
            None
        );
    }

    #[test]
    fn keys_persist_between_runs() {
        let dir = std::env::temp_dir().join(format!("o2a-{}", uuid::Uuid::new_v4().simple()));
        let a = load_or_mint_keys(&dir).unwrap();
        let b = load_or_mint_keys(&dir).unwrap();
        assert_eq!(a.caller_key, b.caller_key);
        assert_ne!(a.caller_key, a.responder_key);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn keys_resolve_with_their_scopes() {
        let keys = Keys {
            caller_key: mint(),
            responder_key: mint(),
        };
        let state = build_state(&keys, "t", None).await;
        let c = state
            .identity
            .resolve_principal(&keys.caller_key)
            .await
            .unwrap();
        let r = state
            .identity
            .resolve_principal(&keys.responder_key)
            .await
            .unwrap();
        assert!(c.api_key.scope.may_call() && !c.api_key.scope.may_respond());
        assert!(r.api_key.scope.may_respond() && !r.api_key.scope.may_call());
        assert_eq!(c.human.id, r.human.id);
    }
}
