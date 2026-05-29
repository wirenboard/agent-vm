//! `agent-vm <agent> [args...]` — boot a per-project sandbox and attach to
//! the chosen agent (or a shell).
//!
//! Phase 2 only knows about env-var auth (`ANTHROPIC_API_KEY`,
//! `OPENAI_API_KEY`); host-rooted refresh-able credentials land in
//! Phase 3/4.

use std::{
    env,
    io::IsTerminal as _,
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use microsandbox::{Sandbox, sandbox::PullPolicy};

use crate::session::ProjectSession;

/// Paths that the guest will tmpfs-mount at boot, wiping anything our
/// `patch` builder baked into the rootfs underneath them. We refuse to mirror
/// a host project rooted here and fall back to `/workspace` instead.
const TMPFS_GUEST_PREFIXES: &[&str] = &["/tmp", "/run", "/dev/shm", "/var/run"];

fn guest_path_is_safe(project: &Path) -> bool {
    let s = match project.to_str() {
        Some(s) => s,
        None => return false,
    };
    !TMPFS_GUEST_PREFIXES
        .iter()
        .any(|p| s == *p || s.starts_with(&format!("{p}/")))
}

/// Absolute directories that must exist inside the guest rootfs before
/// microsandbox can mount the project at its host path. Returns paths from
/// shallowest to deepest, e.g. for `/home/boger/work/foo`:
/// `["/home", "/home/boger", "/home/boger/work", "/home/boger/work/foo"]`.
/// The leaf is included because microsandbox validates `workdir` against the
/// rootfs at create time, *before* the bind mount is materialized — without
/// the empty mount point dir it errors with "workdir does not exist in
/// guest". The bind mount then overlays this empty dir with the host's
/// project contents at boot.
fn mkdir_chain(project: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut acc = PathBuf::new();
    for c in project.components() {
        acc.push(c.as_os_str());
        let s = acc.to_string_lossy().to_string();
        if s != "/" && !s.is_empty() {
            out.push(s);
        }
    }
    out
}

/// Which entry point to attach inside the sandbox.
#[derive(Clone, Copy)]
pub enum Agent {
    Claude,
    Codex,
    Opencode,
    Shell,
}

impl Agent {
    fn command(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Opencode => "opencode",
            Agent::Shell => "bash",
        }
    }

    /// Flags we always pass before the user's own args. The microVM is
    /// the security boundary, so the in-VM agent's "are you sure?"
    /// prompts add no protection and break agent-mode flows.
    ///
    /// `Agent::Shell` carries `-O histappend` so the interactive bash
    /// *appends* its in-memory history to the shared bind-mounted
    /// `~/.bash_history` on exit instead of overwriting it. Without
    /// this, two concurrent `agent-vm shell` invocations in the same
    /// project would have the later-exiting shell wholesale clobber
    /// the earlier shell's commands (the symlink target is the same
    /// host file, see `run.rs`'s `.symlink(... "/root/.bash_history" ...)`).
    fn default_args(self) -> &'static [&'static str] {
        match self {
            Agent::Claude => &["--dangerously-skip-permissions"],
            Agent::Shell => &["-O", "histappend"],
            Agent::Codex | Agent::Opencode => &[],
        }
    }
}

#[derive(ClapArgs)]
pub struct Args {
    /// Sandbox memory in GiB.
    #[arg(long, env = "AGENT_VM_MEMORY_GIB", default_value_t = 2)]
    memory: u32,

    /// vCPU count for the sandbox.
    #[arg(long, env = "AGENT_VM_CPUS", default_value_t = 2)]
    cpus: u8,

    /// Don't inject host gh/git credentials into the guest. With this
    /// set, no gh auth flows through the proxy and the guest agent
    /// can't `git push` / `gh pr create` etc. Useful for one-off
    /// throwaway sessions on a repo you don't trust the agent with.
    #[arg(long = "no-git", default_value_t = false)]
    no_git: bool,

    /// Add a GitHub `owner/repo` slug to the per-launch allow-list
    /// (repeatable). The cwd's `git remote -v` GitHub entries are
    /// always included; use this to widen for cross-repo work.
    #[arg(long = "repo")]
    repo: Vec<String>,

    /// Extra host directories to bind into the guest. Format:
    /// `HOST[:GUEST]`. `GUEST` defaults to `HOST` (mirror at the
    /// same absolute path). Repeatable.
    ///
    /// Each `--mount` consumes one virtio device — libkrun caps the
    /// IRQ pool around 6, so we only have room for a couple of
    /// extras on top of the project + state binds. The launcher will
    /// error clearly if you cross the cap rather than failing at
    /// boot.
    #[arg(long = "mount")]
    mount: Vec<String>,

    /// Publish a guest TCP port to the host. Format:
    /// `[HOST_BIND:]HOST_PORT:GUEST_PORT` (docker-style). HOST_BIND
    /// defaults to `127.0.0.1` — pass `0.0.0.0:HOST_PORT:GUEST_PORT`
    /// to expose on every host interface. Repeatable.
    ///
    /// The guest service must listen on `0.0.0.0` (or the assigned
    /// guest IP from `MSB_NET_IPV4`); a bare `127.0.0.1` bind inside
    /// the guest is not reachable because the smoltcp dial target is
    /// the guest's assigned VLAN address, not loopback.
    #[arg(long = "publish", short = 'p')]
    publish: Vec<String>,

    /// Lima-style host ← guest auto-port-forwarding. The runtime
    /// polls `/proc/net/tcp{,6}` inside the guest every ~2s and
    /// mirrors each detected wildcard (`0.0.0.0`/`[::]`) OR
    /// loopback (`127.0.0.1`/`[::1]`) TCP LISTEN socket onto a
    /// host listener at `127.0.0.1:<same port>` (or an ephemeral
    /// host port if the preferred one is taken). Loopback-only
    /// guest services are reachable via an in-guest agentd
    /// forwarder (`eth0_ip:port → 127.0.0.1:port`) — so anything
    /// listening inside the guest, whether on the wildcard
    /// interface or just loopback, becomes reachable on host
    /// `127.0.0.1`. agent-vm prints each new mapping to stderr as
    /// the runtime emits `PortEvent`s. Off by default.
    ///
    /// Security note: with this flag, every TCP service that
    /// becomes reachable inside the guest is also reachable from
    /// other processes on the host's loopback. If you don't want
    /// that, omit `--auto-publish` and use `--publish` to expose
    /// only the specific ports you mean to share.
    #[arg(long = "auto-publish", default_value_t = false)]
    auto_publish: bool,

    /// Punch a hole through the default egress policy for one IP
    /// or CIDR. Repeatable. Examples:
    ///   `--allow-egress 10.100.1.75`         (single host)
    ///   `--allow-egress 10.100.1.0/24`       (CIDR)
    ///   `--allow-egress fd00::1/128`         (IPv6)
    ///
    /// The default policy (`NetworkPolicy::public_only`) only
    /// allows DNS and the `Public` destination group, so RFC1918
    /// (10/8, 172.16/12, 192.168/16, 100.64/10), loopback, link-
    /// local, and metadata addresses are all denied with
    /// ECONNREFUSED. Use this flag to reach a specific dev box on
    /// the same LAN as the host. Use `--allow-lan` instead if you
    /// want to open the entire Private group at once.
    #[arg(long = "allow-egress")]
    allow_egress: Vec<String>,

    /// Switch the egress policy from `public_only` to `non_local`
    /// — adds the entire `DestinationGroup::Private` (10/8,
    /// 172.16/12, 192.168/16, 100.64/10, fc00::/7) to the allow
    /// list. Coarser than `--allow-egress <CIDR>`; useful for
    /// "trust everything on my LAN". Loopback, link-local, and
    /// metadata are still denied.
    ///
    /// Security note: a compromised in-guest process gets full
    /// access to every device on your LAN with this flag. Prefer
    /// `--allow-egress <CIDR>` for production-ish uses.
    #[arg(long = "allow-lan", default_value_t = false)]
    allow_lan: bool,

    /// Override the OCI image reference. Default:
    /// `ghcr.io/wirenboard/agent-vm-template:latest`. Use a timestamped tag
    /// (`...:YYYY-MM-DDTHH`) to pin a reproducible image.
    #[arg(long, env = "AGENT_VM_IMAGE_TAG")]
    image: Option<String>,

    /// Don't HEAD the registry for a newer manifest digest at
    /// launch (skips the "==> A newer image is available …" banner).
    /// Useful in CI and on flaky networks.
    #[arg(long = "no-update-check", default_value_t = false)]
    no_update_check: bool,

    /// Args forwarded verbatim to the in-sandbox agent command. Use `--` if
    /// any argument starts with `-` to keep clap from claiming it.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    agent_args: Vec<String>,
}

pub async fn launch(agent: Agent, args: Args) -> Result<i32> {
    let session = ProjectSession::for_cwd()?;
    session.ensure_dirs()?;
    // Reap any orphan sandbox dirs left by earlier crashed launchers in
    // this same project before we boot. See
    // `reap_stale_project_sandboxes` for the full rationale.
    reap_stale_project_sandboxes(&session.project_hash).await;
    eprintln!(
        "==> {} in {} (state: {})",
        session.sandbox_name,
        session.project_dir.display(),
        session.state_dir.display(),
    );
    let _ = &session.project_hash;

    let image = args
        .image
        .clone()
        .or_else(|| env::var("AGENT_VM_IMAGE_TAG").ok())
        .unwrap_or_else(|| crate::defaults::DEFAULT_IMAGE_REF.to_string());
    let memory_mib: u32 = args
        .memory
        .checked_mul(1024)
        .context("--memory in GiB overflows u32 MiB")?;
    let cpus = args.cpus;

    // Mount the host project at the *same* absolute path inside the guest so
    // that anything the agent emits (compiler errors, stack traces, git
    // output, file:line references) names a path that's interpretable on the
    // host. The agent-vm-state mount is internal and stays at a fixed path.
    //
    // Exception: paths under tmpfs mount points (typically /tmp, /run,
    // /dev/shm) can't be mirrored, because the guest tmpfs-mounts those at
    // boot — that wipes any mount point our `patch` builder baked into the
    // rootfs. Fall back to /workspace and tell the user once.
    //
    // microsandbox VMs also cap the number of virtio devices via libkrun's
    // IRQ pool; each bind mount is one device on top of the OCI rootfs's two
    // (EROFS lower + ext4 upper). We therefore bind a *single* host
    // directory for all per-agent state and either symlink the agent's
    // expected home (claude, opencode) or redirect via an env var (codex).
    // Codex needs the env-var path because its CLI binary lives under
    // /root/.codex/packages, which a symlink would shadow.
    let host_path = session
        .project_dir
        .to_str()
        .context("project path contains non-UTF-8 bytes; not supported")?;
    let project_guest_path = if guest_path_is_safe(&session.project_dir) {
        host_path.to_string()
    } else {
        eprintln!(
            "==> Project path {host_path} is under a tmpfs mount; mounting at /workspace instead"
        );
        "/workspace".to_string()
    };
    let mut patch_builder_steps = mkdir_chain(Path::new(&project_guest_path));
    // PullPolicy::IfMissing keeps the slow part (pull + materialize) off
    // every launch. We separately HEAD the manifest at the registry via
    // image_check::check_for_update and print a banner if there's a newer
    // image available — the user runs `agent-vm pull` explicitly to
    // fetch it.
    if !args.no_update_check {
        notify_if_update_available(image.as_str()).await;
    }

    // Snapshot host credentials into per-project token files and place
    // placeholder credentials.json files where the in-VM agents will
    // find them. The token files are passed to microsandbox below as
    // SecretValue::File entries; the proxy re-reads them on every
    // connection setup, so a host-side rotation propagates without
    // restarting the sandbox.
    //
    // Passing the in-guest project path lets us pre-approve it in
    // Claude's per-folder trust list (~/.claude.json `projects.<path>.
    // hasTrustDialogAccepted = true`), suppressing the "do you trust
    // this folder?" wizard on first launch in each project.
    // Phase 7 (moved up): parse `--mount HOST[:GUEST]` so the
    // GitHub repo scan below can also walk each mount's remote +
    // submodules — matches main-branch claude-vm.sh behavior. The
    // libkrun IRQ pool is *tight* — empirically the project bind +
    // state bind + network + agentd + OCI overlay already saturate
    // it on this build, so any extra mount tends to trip a
    // confusing `RegisterNetDevice(IrqsExhausted)` at boot. We let
    // the user try and surface a friendly suggestion if libkrun
    // rejects the config; we don't pre-cap because libkrun
    // configurations vary. See discovered upstream issue #3.
    let extra_mounts = parse_extra_mounts(&args.mount).context("parsing --mount")?;
    for em in &extra_mounts {
        if !em.host.exists() {
            anyhow::bail!("--mount host path {:?} does not exist", em.host);
        }
    }
    if !extra_mounts.is_empty() {
        eprintln!(
            "==> Note: {} --mount arg(s) — libkrun's virtio IRQ pool is tight; if you see \
             RegisterNetDevice(IrqsExhausted) at boot, drop a mount or pass --no-git to free a slot.",
            extra_mounts.len()
        );
    }

    // Phase 6: build the per-launch GitHub repo allow-list from the
    // cwd's `git remote -v` + its `.gitmodules` submodules + the
    // same for any `--mount`ed dir, plus any `--repo` overrides.
    // Used both to decide whether to bother capturing host gh auth
    // (no repos → nothing to talk to) and to constrain
    // api.github.com requests server-side via the intercept hook.
    // --no-git suppresses *automatic* cwd-remote detection but does
    // NOT discard explicit --repo arguments (review #11): a user
    // who passes `--no-git --repo X` clearly wants the explicit
    // allow-list. We do warn so they notice if they didn't mean to.
    let mut allowed_repos: Vec<String> = Vec::new();
    if !args.no_git {
        allowed_repos.extend(detect_github_repos(
            &session.project_dir,
            extra_mounts.iter().map(|m| m.host.as_path()),
        ));
    } else if !args.repo.is_empty() {
        eprintln!(
            "==> --no-git skips cwd remote auto-detection, but --repo overrides are kept ({} entr{})",
            args.repo.len(),
            if args.repo.len() == 1 { "y" } else { "ies" },
        );
    }
    for r in &args.repo {
        let r = r.trim().to_string();
        if !r.is_empty() && !allowed_repos.iter().any(|x| x.eq_ignore_ascii_case(&r)) {
            allowed_repos.push(r);
        }
    }
    let use_github = !allowed_repos.is_empty();
    if use_github {
        eprintln!(
            "==> GitHub repo scope ({}): {}",
            allowed_repos.len(),
            allowed_repos.join(", "),
        );
    } else {
        eprintln!("==> GitHub repo scope: <none> (no api.github.com access)");
    }

    let creds = crate::secrets::refresh(&session.state_dir, &project_guest_path, use_github)
        .context("snapshotting host credentials")?;

    // RAII guard so the Phase-5 host-cred mutation check runs on
    // *every* exit path from launch() — including `?` propagation
    // from attach/exec_stream errors (review finding #10). Without
    // this the safety net only fires on the happy path.
    struct SnapshotGuard(Option<crate::secrets::HostCredsSnapshot>);
    impl Drop for SnapshotGuard {
        fn drop(&mut self) {
            if let Some(snap) = self.0.take() {
                crate::secrets::verify_snapshot(&snap);
            }
        }
    }
    let _snap_guard = SnapshotGuard(creds.snapshot.clone());

    // Phase 6/9: always write the guest gitconfig (carries the
    // unconditional `safe.directory = *` so git inside the guest
    // accepts the host-bind-mounted project despite the UID
    // mismatch). The credential-helper / gh hosts.yml stanzas are
    // gated on having actually captured a host gh token.
    //
    // Resolve the host's git author identity (gh api user, then host
    // gitconfig) so in-VM commits land with the user's real
    // name/email rather than the legacy `agent-vm`/`agent-vm@msb.local`
    // placeholder. If neither source yields anything usable, the
    // `[user]` section is omitted and git will refuse to commit
    // until the user sets one — preferable to mis-attribution.
    let host_identity = crate::secrets::discover_host_git_identity();
    if let Some(id) = &host_identity {
        eprintln!(
            "==> Git author identity: {} <{}>{}",
            id.name,
            id.email,
            id.gh_login
                .as_deref()
                .map(|l| format!(" (gh:{l})"))
                .unwrap_or_default(),
        );
    } else {
        eprintln!(
            "==> Git author identity: <none> (gh not logged in and no host gitconfig user.name/email; \
             in-VM `git commit` will refuse until you set one)"
        );
    }
    crate::secrets::write_guest_gh_config(
        &session.state_dir,
        creds.gh_token_file.is_some(),
        host_identity.as_ref(),
    )
    .context("writing guest gh/git config")?;

    let is_local_registry = crate::pull::is_plain_http_registry(&image);
    let mut builder = Sandbox::builder(&session.sandbox_name)
        .image(image.as_str())
        .registry(|r| if is_local_registry { r.insecure() } else { r })
        .pull_policy(PullPolicy::IfMissing)
        .cpus(cpus)
        .memory(memory_mib)
        .workdir(project_guest_path.clone())
        .volume(project_guest_path.clone(), |m| m.bind(&session.project_dir))
        .volume("/agent-vm-state", |m| m.bind(&session.state_dir));
    // Phase 7: extra `--mount HOST[:GUEST]` binds. Each gets its own
    // .volume() — and we also have to mkdir the guest path in the
    // patch builder so microsandbox's workdir/rootfs validation passes
    // (same dance as the project bind above).
    let mut extra_mount_mkdirs: Vec<String> = Vec::new();
    for em in &extra_mounts {
        eprintln!(
            "==> Mounting {} -> {}",
            em.host.display(),
            em.guest.display()
        );
        let host = em.host.clone();
        let guest_str = em
            .guest
            .to_str()
            .context("--mount guest path must be UTF-8")?
            .to_string();
        builder = builder.volume(guest_str.clone(), move |m| m.bind(host.clone()));
        extra_mount_mkdirs.extend(mkdir_chain(&em.guest));
    }
    builder = builder
        .patch(|mut p| {
            for parent in extra_mount_mkdirs.drain(..) {
                p = p.mkdir(parent, None);
            }
            p
        });
    let mut builder = builder
        .patch(|mut p| {
            for parent in patch_builder_steps.drain(..) {
                p = p.mkdir(parent, None);
            }
            p.mkdir("/root/.local", None)
                .mkdir("/root/.local/share", None)
                .mkdir("/root/.config", None)
                .symlink("/agent-vm-state/claude", "/root/.claude", true)
                // Onboarding-state file lives at $HOME root, not in
                // .claude/. Without persistence the in-VM Claude
                // re-runs the theme picker every launch.
                .symlink("/agent-vm-state/claude.json", "/root/.claude.json", true)
                .symlink(
                    "/agent-vm-state/opencode",
                    "/root/.local/share/opencode",
                    true,
                )
                // OpenCode reads its config from $XDG_CONFIG_HOME/opencode/
                // (=~/.config/opencode/), file `opencode.json`. Distinct
                // from the data dir above — wire it separately.
                .symlink(
                    "/agent-vm-state/opencode-config",
                    "/root/.config/opencode",
                    true,
                )
                // Phase 6: gh/git config sits at /root/.gitconfig and
                // /root/.config/gh. write_guest_gh_config writes both
                // into state_dir; these symlinks expose them at the
                // standard paths inside the guest. (Symlink targets
                // are valid only when the underlying file/dir was
                // written; if no gh token was captured, the symlinks
                // dangle but nothing references them.)
                .symlink("/agent-vm-state/gitconfig", "/root/.gitconfig", true)
                .symlink("/agent-vm-state/gh-config", "/root/.config/gh", true)
                // Persistent per-project bash history. secrets::refresh
                // touches `<state>/bash_history` so the symlink target
                // exists on first launch. Bash saves on exit (clean
                // `exit` or Ctrl-D, NOT Ctrl-C of the launcher).
                .symlink(
                    "/agent-vm-state/bash_history",
                    "/root/.bash_history",
                    true,
                )
        })
        .env("CODEX_HOME", "/agent-vm-state/codex");

    // Parse `--publish` into PublishedPort entries up front so we can
    // wire them into the network builder below (the same place that
    // sets up secrets/intercept). Done outside the conditional so it
    // bails early on syntax errors regardless of cred state.
    let publish_ports = parse_publish_args(&args.publish).context("parsing --publish")?;
    for p in &publish_ports {
        eprintln!(
            "==> Publishing host {}:{}/{} → guest :{}",
            p.host_bind,
            p.host_port,
            match p.protocol {
                PublishProto::Tcp => "tcp",
                PublishProto::Udp => "udp",
            },
            p.guest_port,
        );
    }

    // Parse --allow-egress entries into IpNetwork values. Same
    // up-front-bail rule as --publish: syntax errors fail before
    // we boot the sandbox.
    let allow_egress_cidrs =
        parse_allow_egress(&args.allow_egress).context("parsing --allow-egress")?;
    for cidr in &allow_egress_cidrs {
        eprintln!("==> Egress policy: allowing {cidr}");
    }
    if args.allow_lan {
        eprintln!(
            "==> Egress policy: --allow-lan enabled (Private RFC1918 + 100.64/10 + fc00::/7 reachable)"
        );
    }

    // For each provider with a host credential file, register a
    // SecretValue::File secret keyed on the placeholder string the
    // guest will send, then register the OAuth refresh endpoint as a
    // hook target so a 401-then-refresh attempt round-trips through a
    // real host-side rotation (see `intercept_hook`).
    //
    // allow_host covers both the API endpoint and the OAuth endpoint;
    // the OAuth host has to be allowed for the placeholder to leave
    // the VM at all (microsandbox's violation detector would block it
    // otherwise), even though substitution there is a no-op because
    // the body's refresh_token is a placeholder, not a header.
    let has_creds = creds.anthropic_token_file.is_some()
        || creds.openai_token_file.is_some()
        || creds.gh_token_file.is_some();
    let auto_publish = args.auto_publish;
    let allow_lan = args.allow_lan;
    let has_egress_overrides = !allow_egress_cidrs.is_empty() || allow_lan;
    if has_creds || !publish_ports.is_empty() || auto_publish || has_egress_overrides {
        use crate::secrets::*;
        let anthropic = creds.anthropic_token_file.clone();
        let openai = creds.openai_token_file.clone();
        let opencode = creds.opencode_openai_access_token_file.clone();
        let gh = creds.gh_token_file.clone();
        let has_gh = gh.is_some();
        let allowed_repos_for_hook = allowed_repos.clone();
        let self_path = std::env::current_exe().context("std::env::current_exe")?;
        let state_dir = session.state_dir.clone();
        let publish_ports_for_net = publish_ports.clone();
        let allow_egress_for_net = allow_egress_cidrs.clone();
        builder = builder.network(move |mut n| {
            n = n.tls(|t| t);
            for p in &publish_ports_for_net {
                let host_bind = p.host_bind;
                n = match p.protocol {
                    PublishProto::Tcp => n.port_bind(host_bind, p.host_port, p.guest_port),
                    PublishProto::Udp => n.port_udp_bind(host_bind, p.host_port, p.guest_port),
                };
            }
            if auto_publish {
                n = n.auto_publish();
            }
            if allow_lan {
                use microsandbox::microsandbox_network::policy::DestinationGroup;
                n = n.allow_egress_group(DestinationGroup::Private);
            }
            for cidr in &allow_egress_for_net {
                n = n.allow_egress_cidr(*cidr);
            }
            if !has_creds {
                return n;
            }
            // We only ever substitute into Authorization: Bearer headers.
            // Explicitly disable basic_auth so the proxy's per-chunk fast
            // path can short-circuit when the placeholder isn't present
            // — critical for post-WebSocket-upgrade binary frames where
            // a UTF-8 lossy round trip would corrupt the bytes.
            if let Some(file) = anthropic {
                n = n.secret(|s| {
                    s.env("MSB_AGENT_VM_ANTHROPIC_UNUSED")
                        .value(file)
                        .placeholder(ANTHROPIC_ACCESS_PLACEHOLDER)
                        .inject_basic_auth(false)
                        .allow_host(ANTHROPIC_API_HOST)
                        .allow_host(ANTHROPIC_OAUTH_HOST)
                        .allow_host(ANTHROPIC_MCP_PROXY_HOST)
                });
            }
            if let Some(file) = openai {
                n = n.secret(|s| {
                    s.env("MSB_AGENT_VM_OPENAI_UNUSED")
                        .value(file)
                        .placeholder(OPENAI_ACCESS_PLACEHOLDER)
                        .inject_basic_auth(false)
                        .allow_host(OPENAI_API_HOST)
                        .allow_host(OPENAI_CHATGPT_HOST)
                        .allow_host(OPENAI_OAUTH_HOST)
                });
            }
            // OpenCode sends Authorization: Bearer <synthetic JWT> to
            // api.openai.com; the proxy swaps the JWT for the real
            // OpenAI access token (same on-disk file as Codex uses).
            if let Some(file) = opencode {
                n = n.secret(|s| {
                    s.env("MSB_AGENT_VM_OPENCODE_OPENAI_UNUSED")
                        .value(file)
                        .placeholder(OPENCODE_OPENAI_ACCESS_PLACEHOLDER)
                        .inject_basic_auth(false)
                        .allow_host(OPENAI_API_HOST)
                        .allow_host(OPENAI_CHATGPT_HOST)
                });
            }
            // Phase 6: gh CLI sends `Authorization: token <token>` (or
            // Bearer); git uses Basic auth (base64(x-access-token:tok)).
            // `inject_basic_auth(true)` covers the Basic-auth path. We
            // accept the perf hit (basic_auth disables the per-chunk
            // fast path) because GitHub connections aren't WebSocket.
            //
            // The per-launch repo allow-list binds api.github.com
            // (full-buffer hook below) AND the git smart-HTTP hosts
            // (streaming `rule_streaming` rules below). The smart-
            // HTTP path uses microsandbox's headers-only dispatch
            // primitive — the hook decides based on the request
            // line alone, so multi-MB git push pack data never hits
            // the 64 KiB intercept buffer.
            if let Some(file) = gh {
                n = n.secret(|s| {
                    s.env("MSB_AGENT_VM_GH_UNUSED")
                        .value(file)
                        .placeholder(GH_TOKEN_PLACEHOLDER)
                        .inject_basic_auth(true)
                        .allow_host(GITHUB_API_HOST)
                        .allow_host(GITHUB_HOST)
                        .allow_host(GITHUB_CODELOAD_HOST)
                        .allow_host(GITHUB_RAW_HOST)
                        .allow_host(GITHUB_OBJECTS_HOST)
                });
            }
            n.intercept(|i| {
                let mut hook_argv: Vec<String> = vec![
                    self_path.to_string_lossy().to_string(),
                    "_intercept-hook".to_string(),
                    "--state-dir".to_string(),
                    state_dir.to_string_lossy().to_string(),
                ];
                for repo in &allowed_repos_for_hook {
                    hook_argv.push("--allowed-repo".to_string());
                    hook_argv.push(repo.clone());
                }
                let mut ix = i
                    .hook(hook_argv)
                    .rule(ANTHROPIC_OAUTH_HOST, "POST", ANTHROPIC_OAUTH_TOKEN_PATH)
                    .rule(OPENAI_OAUTH_HOST, "POST", OPENAI_OAUTH_TOKEN_PATH);
                // Phase 6: intercept every method gh CLI uses on
                // api.github.com so the hook can enforce the repo
                // allow-list. Path "/" matches everything (prefix
                // semantics in handler.rs). Only fires when we have
                // a host gh token; otherwise the hook has nothing to
                // forward with and there's no risk of a leak.
                if has_gh {
                    for method in ["GET", "POST", "PATCH", "PUT", "DELETE"] {
                        ix = ix.rule(GITHUB_API_HOST, method, "/");
                    }
                    // Phase 6 round 2: streaming intercept for the git
                    // smart-HTTP protocol on github.com. The hook
                    // decides based on the request line alone (path
                    // is `/<owner>/<repo>.git/...`); empty stdout =
                    // passthrough → secret-substitution layer fills
                    // in the real bearer for legitimate push/clone.
                    // Non-empty = synthesized 403 for off-allow-list
                    // repos. Without this the per-launch repo
                    // allow-list would only bind api.github.com and
                    // git push could reach anywhere.
                    for method in ["GET", "POST"] {
                        ix = ix.rule_streaming(GITHUB_HOST, method, "/");
                        ix = ix.rule_streaming(GITHUB_CODELOAD_HOST, method, "/");
                        ix = ix.rule_streaming(GITHUB_RAW_HOST, method, "/");
                        ix = ix.rule_streaming(GITHUB_OBJECTS_HOST, method, "/");
                    }
                }
                ix
            })
        });
    }

    // Still honour ANTHROPIC_API_KEY / OPENAI_API_KEY if explicitly set
    // by the user — that path stays a simple Bearer header, no
    // placeholder substitution involved.
    for var in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
        if let Ok(val) = env::var(var) {
            if !val.is_empty() {
                builder = builder.env(var, val);
            }
        }
    }
    // The image's PATH was set inside the Dockerfile, but it lives in the
    // shell rc files of /root. attach() launches the agent directly via
    // execve, so re-publish the same PATH here.
    //
    // `/usr/sbin` is here because dockerd, runc, iptables and docker-proxy
    // live there in debian, and dockerd does PATH lookups for its helper
    // binaries at runtime (not just at exec). Keep this list in sync with
    // the `ENV PATH=…` in images/Dockerfile.
    builder = builder.env(
        "PATH",
        "/root/.local/bin:/root/.claude/local/bin:/root/.opencode/bin:/usr/local/bin:/usr/bin:/usr/sbin:/bin",
    );

    // Claude Code refuses to run as root with --dangerously-skip-permissions
    // unless this env var is set. The whole point of running it in a
    // microVM is that the sandbox IS our security boundary, so the
    // in-guest CLI's extra guard would just block us from getting work
    // done. Same env var the original Bash agent-vm used.
    builder = builder.env("IS_SANDBOX", "1");

    let profile = env::var("AGENT_VM_PROFILE").is_ok();
    eprintln!(
        "==> Booting sandbox from {image} ({memory_mib} MiB, {cpus} vCPU; first run pulls layers, otherwise ~3s)"
    );
    let t_create = Instant::now();
    let config = builder.build().await.context("preparing sandbox config")?;
    if env::var("AGENT_VM_DEBUG_CONFIG").is_ok() {
        eprintln!(
            "[debug] sandbox config JSON: {}",
            serde_json::to_string_pretty(&config).unwrap_or_default()
        );
    }
    let (progress, task) = Sandbox::create_with_pull_progress(config);
    let render_task = tokio::spawn(crate::pull_progress::render(progress));
    // See pull.rs: await render before propagating errors so finish()
    // clears the bars, and use the logging helper so render-task panics
    // are visible instead of silently swallowed.
    let result = task
        .await
        .context("create-with-pull-progress join")
        .and_then(|inner| inner.context("creating sandbox"));
    crate::pull_progress::await_render(render_task).await;
    let sandbox = result?;
    if profile {
        eprintln!("[profile] create: {:?}", t_create.elapsed());
    }

    // Confirm the image's API contract matches what this binary
    // expects. Out-of-range → clear actionable error instead of
    // mysterious mount-not-found / agent-crashing-on-startup
    // failures inside the VM.
    crate::image_api_version::check(&sandbox)
        .await
        .context("verifying image-API contract version")?;

    // Subscribe to the runtime's auto-publish event stream and
    // surface each mapping to the user. Spawned regardless of
    // whether auto-publish is enabled — when disabled the runtime
    // never emits events, so the subscriber just idles. Cheap.
    if args.auto_publish {
        eprintln!("==> auto-publish: watching guest LISTEN sockets via /proc/net/tcp{{,6}}");
        let sb_for_events = sandbox.clone();
        tokio::spawn(async move {
            let mut events = sb_for_events.port_events().await;
            use microsandbox::protocol::network::PortEvent;
            while let Some(event) = events.recv().await {
                match event {
                    PortEvent::Added {
                        host_bind,
                        host_port,
                        guest_port,
                    } => {
                        eprintln!(
                            "==> auto-publish: guest :{guest_port} → host {host_bind}:{host_port}"
                        );
                    }
                    PortEvent::Removed {
                        host_bind,
                        host_port,
                        guest_port,
                    } => {
                        eprintln!(
                            "==> auto-publish: guest :{guest_port} closed (released {host_bind}:{host_port})"
                        );
                    }
                }
            }
        });
    }

    let inner_cmd = agent.command();
    // Prepend agent-vm's default flags (e.g. --dangerously-skip-permissions
    // for Claude) unless the user already provided them.
    let mut inner_args: Vec<String> = agent
        .default_args()
        .iter()
        .filter(|d| !args.agent_args.iter().any(|u| u == *d))
        .map(|s| s.to_string())
        .collect();
    if matches!(agent, Agent::Shell) && !args.agent_args.is_empty() {
        // `agent-vm shell foo bar` runs `foo bar` as a command. Without
        // `-c`, bash treats the first non-option positional as a script
        // filename and PATH-searches for it, so `agent-vm shell ls` lands
        // on `/usr/bin/ls` and prints "cannot execute binary file" the
        // moment bash hits the ELF magic. Joining the user's args into a
        // single `-c` command line (each arg shell-escaped so quoting is
        // preserved across the boundary) is the standard fix.
        let cmd = args
            .agent_args
            .iter()
            .map(|a| shell_escape(a))
            .collect::<Vec<_>>()
            .join(" ");
        inner_args.push("-c".into());
        inner_args.push(cmd);
    } else {
        inner_args.extend(args.agent_args);
    }

    // Wrap the agent invocation in a tiny bash prelude that:
    //
    // 1. Strips IPv6 nameservers from /etc/resolv.conf before exec'ing
    //    the agent. microsandbox's agentd writes both v4 and v6 gateway
    //    DNS into the guest's /etc/resolv.conf at boot. The v6 entry was
    //    observed unresponsive in at least one nested-libkrun setup
    //    (gateway times out on v6 DNS queries), and codex's Rust async
    //    resolver returns EAI_AGAIN ("Try again") in that case instead
    //    of falling through to the working v4 resolver the way glibc's
    //    getaddrinfo does. Result: codex hangs at startup with "failed
    //    to lookup address information" for chatgpt.com, even though
    //    `getent hosts chatgpt.com` returns immediately. Stripping the
    //    v6 nameserver line makes the resolver single-stack, which is
    //    fine for outbound traffic to public APIs. The regex matches
    //    lines whose nameserver value contains a colon — IPv4 addresses
    //    never do, IPv6 addresses always do.
    //
    // 2. Redirects stdin to /dev/null when not on a TTY. exec_with's
    //    default `StdinMode::Null` was observed *not* to satisfy codex
    //    0.133's `exec` subcommand: codex blocks indefinitely on what it
    //    thinks is unbounded interactive input. Backgrounding codex (`&`)
    //    fixed it (bash auto-redirects stdin to /dev/null for background
    //    jobs) but we can't background the user's agent. An explicit
    //    `exec < /dev/null` gives codex a real /dev/null fd and it
    //    proceeds. `[ -t 0 ]` keeps interactive TTY launches unaffected.
    // Phase 9 adds a project-runtime hook (`.agent-vm.runtime.sh`):
    // if the file exists at the project root inside the guest, source
    // it before exec'ing the agent. Project owners use this for
    // setup that has to happen *inside* the sandbox (npm install,
    // docker compose up, env-var exports). Runs once per launch with
    // PWD set to the project dir; non-zero exit aborts the launch
    // with the same exit code.
    // 3. Adds the microsandbox MITM CA to the `chrome` user's NSS DB
    //    so chromium (launched by chrome-devtools-mcp under that user
    //    via /usr/local/bin/agent-vm-chrome-mcp) verifies the
    //    intercepted TLS chain instead of failing every HTTPS page
    //    with ERR_CERT_AUTHORITY_INVALID. The CA file
    //    `/usr/local/share/ca-certificates/microsandbox-ca.crt` is
    //    written into the guest by agentd at boot, so this can't be
    //    baked into the image — the per-boot CA is what we have to
    //    inject. Trust string is `-t C,,` (trusted issuer of *server*
    //    certs only; the leading `T` would also mark it as a trusted
    //    issuer of client certs, which we don't want). Only injected
    //    when the chrome MCP is enabled: gating both here and in
    //    `secrets::write_default_claude_root_state` on the same env
    //    var keeps the opt-out actually opt-out — no sudo, no
    //    certutil fork.
    let project_guest_path_escaped = shell_escape(&project_guest_path);
    // Env-var semantics: `AGENT_VM_NO_CHROME_MCP` set to *any* value
    // (including empty) opts out. Matches `AGENT_VM_PROFILE` /
    // `AGENT_VM_DEBUG_CONFIG` in the same codebase. Unconventional vs
    // "VAR=0 means off" — documented here so the next reader doesn't
    // re-flag it.
    let chrome_mcp_prelude = if std::env::var_os("AGENT_VM_NO_CHROME_MCP").is_some() {
        String::new()
    } else {
        // Image-fresh NSS DB starts empty on every launch (the sandbox
        // name carries the launcher PID — see session.rs — so every
        // invocation creates a fresh rootfs upper layer rather than
        // reusing a prior one), so `certutil -A` always runs against
        // the same baseline; no chance of accumulating duplicate trust
        // entries across boots.
        //
        // Failure modes worth surfacing (vs silently swallowing with
        // `|| true` like the original Phase 7 patch): sudoers
        // dropped/world-writable, NSS DB corrupted, CA file missing
        // because agentd's TLS init didn't run, or chrome user
        // removed in a downstream image. Without a warning, every
        // chrome MCP HTTPS request would return
        // `ERR_CERT_AUTHORITY_INVALID` with no breadcrumb back to the
        // launcher. Stderr is captured in a temp file and tail'd on
        // failure so the user sees the actual certutil/sudo error
        // rather than a generic "non-zero exit".
        String::from(
            "if [ -f /usr/local/share/ca-certificates/microsandbox-ca.crt ] \\\n\
                     && [ -d /home/chrome/.pki/nssdb ]; then\n\
             \t_cu_err=$(mktemp)\n\
             \tif ! sudo -u chrome -n -- certutil -d sql:/home/chrome/.pki/nssdb -A \\\n\
             \t\t-t C,, -n microsandbox \\\n\
             \t\t-i /usr/local/share/ca-certificates/microsandbox-ca.crt 2>\"$_cu_err\"; then\n\
             \t\techo \"==> warning: failed to install microsandbox CA into chrome NSS DB;\" >&2\n\
             \t\techo \"==>          HTTPS in chrome-devtools MCP will fail with ERR_CERT_AUTHORITY_INVALID.\" >&2\n\
             \t\techo \"==>          certutil/sudo stderr:\" >&2\n\
             \t\tsed 's/^/==>          /' \"$_cu_err\" >&2\n\
             \tfi\n\
             \trm -f \"$_cu_err\"\n\
             fi\n",
        )
    };
    let prelude = format!(
        "sed -i '/^nameserver .*:/d' /etc/resolv.conf 2>/dev/null || true\n\
         [ -t 0 ] || exec < /dev/null\n\
         {chrome_mcp_prelude}\
         _hook={path}/.agent-vm.runtime.sh\n\
         if [ -f \"$_hook\" ]; then\n\
         \techo \"==> sourcing $_hook\" >&2\n\
         \tcd {path} && . \"$_hook\" || {{ rc=$?; echo \"==> .agent-vm.runtime.sh failed (exit $rc)\" >&2; exit $rc; }}\n\
         fi",
        path = project_guest_path_escaped,
    );
    let prelude = prelude.as_str();
    let mut shell_line = String::from(prelude);
    shell_line.push_str("; exec ");
    shell_line.push_str(&shell_escape(inner_cmd));
    for a in &inner_args {
        shell_line.push(' ');
        shell_line.push_str(&shell_escape(a));
    }
    let cmd = "bash";
    let agent_args: Vec<String> = vec!["-c".into(), shell_line];

    let t_run = Instant::now();
    let exit = if std::io::stdin().is_terminal() {
        eprintln!("==> Attaching to {inner_cmd}");
        sandbox
            .attach(cmd, agent_args)
            .await
            .with_context(|| {
                format!(
                    "attaching to {inner_cmd} (full logs: {})",
                    sandbox_log_dir(&session.sandbox_name).display()
                )
            })?
    } else {
        // No host TTY (piped, redirected, smoke-tested under `sg`/`sudo` etc.).
        // attach() needs a real /dev/tty for raw-mode stdin, so use the
        // streaming exec API instead: write stdout/stderr to ours as they
        // arrive. That keeps progress visible on long-running agent
        // commands (codex exec can take >30s for a single response) and
        // lets us inspect partial output when the user Ctrl-Cs or the
        // shell times out.
        eprintln!("==> Running {inner_cmd} in sandbox (no TTY; streaming output)");
        use microsandbox::sandbox::exec::ExecEvent;
        use tokio::io::AsyncWriteExt as _;
        let mut handle = sandbox
            .exec_stream_with(cmd, |e| {
                e.args(agent_args).cwd(project_guest_path.clone())
            })
            .await
            .with_context(|| {
                format!(
                    "running {inner_cmd} in sandbox (full logs: {})",
                    sandbox_log_dir(&session.sandbox_name).display()
                )
            })?;
        let mut stdout = tokio::io::stdout();
        let mut stderr = tokio::io::stderr();
        // Track whether we actually saw an Exited event. The previous
        // `let mut code = 1` conflated "stream closed without Exited"
        // (infra failure) with "agent exited with 1" (real failure) —
        // CI couldn't tell them apart. Review finding #9. Now we
        // return Err on premature stream close so the launcher
        // bubbles up an actionable error.
        let mut exit_code: Option<i32> = None;
        while let Some(event) = handle.recv().await {
            match event {
                ExecEvent::Stdout(b) => {
                    stdout.write_all(&b).await.ok();
                    stdout.flush().await.ok();
                }
                ExecEvent::Stderr(b) => {
                    stderr.write_all(&b).await.ok();
                    stderr.flush().await.ok();
                }
                ExecEvent::Exited { code: c } => {
                    exit_code = Some(c);
                    break;
                }
                ExecEvent::Failed(payload) => {
                    anyhow::bail!(
                        "exec session failed: {payload:?} (full logs: {})",
                        sandbox_log_dir(&session.sandbox_name).display()
                    );
                }
                ExecEvent::Started { .. } | ExecEvent::StdinError(_) => {}
            }
        }
        match exit_code {
            Some(c) => c,
            None => anyhow::bail!(
                "exec session event stream ended without Exited (agentd disconnect or microsandbox bug; partial output above; full logs: {})",
                sandbox_log_dir(&session.sandbox_name).display()
            ),
        }
    };

    if profile {
        eprintln!("[profile] run:    {:?}", t_run.elapsed());
    }

    eprintln!("==> Stopping sandbox");
    let t_stop = Instant::now();
    sandbox.stop_and_wait().await.ok();
    if profile {
        eprintln!("[profile] stop:   {:?}", t_stop.elapsed());
    }
    let t_remove = Instant::now();
    Sandbox::remove(&session.sandbox_name).await.ok();
    if profile {
        eprintln!("[profile] remove: {:?}", t_remove.elapsed());
    }

    // Phase 5 safety net (host-cred mutation check) runs via the
    // SnapshotGuard above, which drops at end of scope including
    // any `?`-propagated error path.

    Ok(exit)
}

/// Best-effort hint at where msb writes a sandbox's per-launch
/// logs. The directory contains three files worth checking on a
/// failed launch:
///
/// - `runtime.log` — msb tracing + Rust panic from the sandbox
///   subprocess (vendor `vm.rs::setup_log_capture` redirects its
///   stderr here).
/// - `kernel.log` — kernel printk / early-init panic (vendor
///   `vm.rs::setup_kernel_log`).
/// - `boot-error.json` — structured cause when the VM fails to come
///   up far enough to write into the two above (vendor
///   `boot_error.rs` is the canonical source of "couldn't boot
///   because X").
///
/// Returning a *directory* rather than a single file lets the user
/// `ls` it and pick whichever is non-empty, instead of following a
/// `runtime.log` hint that's zero bytes on early-boot failures.
///
/// Resolution mostly matches upstream's
/// `microsandbox_utils::resolve_home()` (`$MSB_HOME` →
/// `$HOME/.microsandbox` → `./.microsandbox`), with one deliberate
/// difference: when both `MSB_HOME` and `HOME` are unset (cron,
/// systemd-unit-without-Environment=HOME, `env -i`) we canonicalize
/// the relative fallback against the current working directory so
/// the hint string is absolute. Upstream does the same lookup
/// inside the msb subprocess where CWD is more controlled; on the
/// launcher side, embedding `./.microsandbox` in a user-facing
/// error message rendered far from where the launcher ran is
/// confusing.
fn sandbox_log_dir(sandbox_name: &str) -> PathBuf {
    microsandbox_sandboxes_root().join(sandbox_name).join("logs")
}

/// `$MSB_HOME/sandboxes` (resolved through the same ladder as
/// [`sandbox_log_dir`]). Holds one subdirectory per sandbox that
/// microsandbox materialized — including the `upper.ext4` overlay,
/// `logs/`, and other on-disk state.
fn microsandbox_sandboxes_root() -> PathBuf {
    let home = env::var_os("MSB_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".microsandbox")))
        .unwrap_or_else(|| {
            env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".microsandbox")
        });
    home.join("sandboxes")
}

/// Best-effort GC of orphan sandbox dirs from earlier crashed launches
/// in this same project (matching name prefix `agent-vm-<hash>-`).
///
/// The Sandbox name now carries the launcher's PID (see
/// [`crate::session::ProjectSession`]) so two concurrent invocations
/// don't collide — but the previous deterministic name doubled as
/// implicit garbage collection: the next launch's `.replace()` would
/// stop+kill any leftover same-name sandbox and unlink its on-disk
/// state. With per-launch names that GC never fires; if a launcher
/// crashes between `Sandbox::create` and the cleanup `Sandbox::remove`
/// at the end of [`launch`], its `~/.microsandbox/sandboxes/agent-vm-
/// <hash>-<pid>/` (overlay + logs + DB row) leaks forever. We reap
/// those here.
///
/// Safety: we only remove entries whose PID is *not currently alive*
/// (checked via `/proc/<pid>`), so a peer launcher running right now —
/// the very case this PR enables — is never touched. The reap also
/// proactively clears any stale entry whose PID we're about to reuse
/// ourselves: otherwise `Sandbox::create` would fail with "already
/// exists" because we dropped `.replace()`. PID reuse for our own
/// `process::id()` is rare but not impossible after enough crashed
/// launches accumulate.
async fn reap_stale_project_sandboxes(project_hash: &str) {
    let root = microsandbox_sandboxes_root();
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(_) => return, // no microsandbox home yet — nothing to reap
    };
    let prefix = format!("agent-vm-{project_hash}-");
    for entry in entries.flatten() {
        let name_os = entry.file_name();
        let name = match name_os.to_str() {
            Some(s) => s,
            None => continue,
        };
        let pid_str = match name.strip_prefix(&prefix) {
            Some(s) => s,
            None => continue,
        };
        let pid: u32 = match pid_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if pid_alive(pid) {
            // Either a peer launcher running right now, or — if it's
            // our own PID — we're about to claim the same name and
            // `Sandbox::remove` would destroy what we're booting.
            continue;
        }
        // Best-effort. `Sandbox::remove` handles "doesn't exist in DB
        // but dir present" by still removing the dir; conversely if
        // it's gone from disk but a DB row lingers it cleans that too.
        // Either way, errors here are not fatal: worst case the user
        // sees the dir again next launch.
        let _ = microsandbox::Sandbox::remove(name).await;
    }
}

fn pid_alive(pid: u32) -> bool {
    // /proc/<pid> exists iff a process with that PID is currently
    // alive. We don't try to disambiguate "alive but a different
    // program reusing this PID" — if /proc/<pid> exists we conservatively
    // skip the reap. The cost of leaving one stale dir until the
    // process exits is small compared to the cost of nuking an
    // unrelated process's sandbox.
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// One `--mount HOST[:GUEST]` argument resolved into separate paths.
struct ExtraMount {
    host: PathBuf,
    guest: PathBuf,
}

/// One `--publish [HOST_BIND:]HOST_PORT:GUEST_PORT[/proto]` entry, parsed.
#[derive(Clone, Debug)]
struct PublishPort {
    host_bind: std::net::IpAddr,
    host_port: u16,
    guest_port: u16,
    protocol: PublishProto,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublishProto {
    Tcp,
    /// Reserved for when the underlying PortPublisher gains UDP
    /// support. Parser currently rejects `/udp` so this variant
    /// is unreachable from user input, but kept so the eventual
    /// enable change is a single-site edit.
    #[allow(dead_code)]
    Udp,
}

/// Parse `--publish` entries. Accepts docker-style:
///   `HOST_PORT:GUEST_PORT`                 → 127.0.0.1 bind, TCP
///   `HOST_IP:HOST_PORT:GUEST_PORT`         → explicit IPv4 bind
///   `[HOST_IP6]:HOST_PORT:GUEST_PORT`      → explicit IPv6 bind (bracket form)
///   any of the above + `/tcp` (UDP rejected — not implemented)
///
/// The connection enters the smoltcp in-process stack as a dial to
/// the guest's assigned MSB_NET_IPV4 (or v6) on `GUEST_PORT`, so the
/// in-guest service has to listen on `0.0.0.0`/`::` (or that exact
/// guest IP) — a bare `127.0.0.1` bind inside the guest is not
/// reachable from the publisher.
///
/// `/udp` is parsed but REJECTED with a clear error: the underlying
/// `PortPublisher::spawn_listener_one` short-circuits non-TCP ports
/// silently, so silently accepting `/udp` would leave the user with
/// a "published" port that has no listener. When UDP support lands
/// upstream, drop the rejection here.
fn parse_publish_args(raw: &[String]) -> Result<Vec<PublishPort>> {
    use std::net::IpAddr;
    let mut out = Vec::with_capacity(raw.len());
    for entry in raw {
        let (body, proto) = match entry.rsplit_once('/') {
            Some((b, p)) if matches!(p, "tcp" | "udp" | "TCP" | "UDP") => {
                (b, p.to_ascii_lowercase())
            }
            _ => (entry.as_str(), "tcp".to_string()),
        };
        if proto == "udp" {
            anyhow::bail!(
                "--publish {entry:?}: UDP is not yet supported by the underlying smoltcp \
                 PortPublisher; remove `/udp` to publish a TCP port instead"
            );
        }
        let protocol = PublishProto::Tcp;

        // Split off an IPv6 bracketed prefix first (docker convention)
        // so `[::1]:8080:80` doesn't trip the generic colon split.
        let (host_bind, rest) = if let Some(after_bracket) = body.strip_prefix('[') {
            let (v6_str, after) = after_bracket.split_once("]:").ok_or_else(|| {
                anyhow::anyhow!(
                    "--publish {entry:?}: bracketed IPv6 must be `[ADDR]:HOST_PORT:GUEST_PORT`"
                )
            })?;
            let addr = v6_str.parse::<std::net::Ipv6Addr>().with_context(|| {
                format!("--publish {entry:?}: HOST_BIND {v6_str:?} is not an IPv6")
            })?;
            (Some(IpAddr::V6(addr)), after)
        } else {
            (None, body)
        };

        let parts: Vec<&str> = rest.split(':').collect();
        let (host_bind, host_port, guest_port) = match (host_bind, parts.as_slice()) {
            (Some(bind), [h, g]) => (
                bind,
                parse_port(entry, "HOST_PORT", h)?,
                parse_port(entry, "GUEST_PORT", g)?,
            ),
            (None, [h, g]) => (
                IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                parse_port(entry, "HOST_PORT", h)?,
                parse_port(entry, "GUEST_PORT", g)?,
            ),
            (None, [ip, h, g]) => (
                ip.parse::<IpAddr>().with_context(|| {
                    format!("--publish {entry:?}: HOST_BIND {ip:?} is not an IP")
                })?,
                parse_port(entry, "HOST_PORT", h)?,
                parse_port(entry, "GUEST_PORT", g)?,
            ),
            _ => anyhow::bail!(
                "--publish {entry:?} must be [HOST_BIND:]HOST_PORT:GUEST_PORT or \
                 [IPv6_BIND]:HOST_PORT:GUEST_PORT"
            ),
        };
        if host_port == 0 || guest_port == 0 {
            anyhow::bail!("--publish {entry:?}: port 0 is not allowed");
        }
        out.push(PublishPort {
            host_bind,
            host_port,
            guest_port,
            protocol,
        });
    }
    Ok(out)
}

fn parse_port(entry: &str, field: &str, s: &str) -> Result<u16> {
    s.parse::<u16>()
        .with_context(|| format!("--publish {entry:?}: {field} {s:?} is not a u16"))
}

/// Parse `--allow-egress` entries. Each entry is an IP literal or
/// a CIDR (e.g. `10.100.1.75` or `10.100.1.0/24`). A bare IP is
/// expanded to a /32 (v4) or /128 (v6) CIDR — that matches the
/// shape the policy builder's `Destination::Cidr` expects.
fn parse_allow_egress(raw: &[String]) -> Result<Vec<ipnetwork::IpNetwork>> {
    let mut out = Vec::with_capacity(raw.len());
    for entry in raw {
        // Try CIDR first (foo/N); fall back to bare IP.
        let cidr = if entry.contains('/') {
            entry
                .parse::<ipnetwork::IpNetwork>()
                .with_context(|| format!("--allow-egress {entry:?}: not a valid CIDR"))?
        } else {
            let ip: std::net::IpAddr = entry.parse().with_context(|| {
                format!("--allow-egress {entry:?}: not an IP address or CIDR")
            })?;
            // /32 for v4, /128 for v6 — single-host rule.
            ipnetwork::IpNetwork::from(ip)
        };
        out.push(cidr);
    }
    Ok(out)
}

/// Parse the raw `--mount` argv strings into `(host, guest)` pairs.
/// `HOST` alone defaults `GUEST` to the same absolute path (mirror).
/// `HOST:GUEST` lets you remap. Errors clearly on absolute-path
/// requirements (relative paths are confusing across host/guest).
fn parse_extra_mounts(raw: &[String]) -> Result<Vec<ExtraMount>> {
    let mut out = Vec::with_capacity(raw.len());
    for entry in raw {
        let (host_s, guest_s) = match entry.split_once(':') {
            Some((h, g)) => (h.trim().to_string(), g.trim().to_string()),
            None => (entry.trim().to_string(), entry.trim().to_string()),
        };
        if host_s.is_empty() || guest_s.is_empty() {
            anyhow::bail!("--mount value {entry:?} must be HOST[:GUEST] (non-empty)");
        }
        let host = PathBuf::from(&host_s);
        let guest = PathBuf::from(&guest_s);
        if !host.is_absolute() {
            anyhow::bail!("--mount host path {host_s:?} must be absolute");
        }
        if !guest.is_absolute() {
            anyhow::bail!("--mount guest path {guest_s:?} must be absolute");
        }
        // Canonicalize host so we follow symlinks; the bind target
        // needs to be a real path on the host.
        let host = host
            .canonicalize()
            .with_context(|| format!("canonicalizing --mount host {host_s:?}"))?;
        out.push(ExtraMount { host, guest });
    }
    Ok(out)
}

/// Build the GitHub repo allow-list by scanning `project_dir` and
/// each `extra_mount_dir` for:
///   * every `git remote -v` URL that points at github.com,
///   * every github.com URL listed in that dir's `.gitmodules`.
/// Matches main-branch claude-vm.sh:1463-1514 — without this,
/// `git push` from inside a submodule (or a mounted sibling repo)
/// would hit api.github.com anonymous and 401.
///
/// Failure modes (not a repo, no `.gitmodules`, non-github remote)
/// just contribute nothing — caller passes `--repo` to widen.
fn detect_github_repos<'a>(
    project_dir: &Path,
    extra_mount_dirs: impl IntoIterator<Item = &'a Path>,
) -> Vec<String> {
    let mut slugs: Vec<String> = Vec::new();
    scan_dir_for_github_slugs(project_dir, &mut slugs);

    // Dedup against the project dir so a mount that *is* the
    // project dir (or a symlink to it) doesn't re-run the same scan.
    let project_canon = project_dir.canonicalize().ok();
    for dir in extra_mount_dirs {
        if let (Some(pc), Ok(mc)) = (project_canon.as_ref(), dir.canonicalize()) {
            if &mc == pc {
                continue;
            }
        }
        scan_dir_for_github_slugs(dir, &mut slugs);
    }
    slugs
}

/// Scan one directory: top-level remotes + one level of submodule
/// URLs in `.gitmodules`. Submodule scanning is shallow (matches
/// claude-vm.sh; recursing into each submodule's `.gitmodules`
/// would balloon scope and add little value in practice).
fn scan_dir_for_github_slugs(dir: &Path, out: &mut Vec<String>) {
    for slug in parse_dir_remote_github_slugs(dir) {
        push_slug_unique(out, slug);
    }
    for slug in parse_gitmodules_github_slugs(dir) {
        push_slug_unique(out, slug);
    }
}

fn push_slug_unique(out: &mut Vec<String>, slug: String) {
    if !out.iter().any(|x| x.eq_ignore_ascii_case(&slug)) {
        out.push(slug);
    }
}

/// `git -C <dir> remote -v` → github slugs. Hardened against a
/// hostile cwd that might define core.fsmonitor / aliases (review
/// #6): the user may have just cloned a project they don't fully
/// trust; running git in that repo before we've even built the
/// sandbox would otherwise honour repo-local hooks — host RCE
/// pre-sandbox. Disable the dangerous knobs and force
/// safe.directory so we don't fail closed on a foreign-UID
/// checkout. (Note: `-c include.path=` isn't valid git syntax;
/// includeIf/include are only honored from files git already
/// decides to read, which `-c` flags can't suppress in 2.x, so we
/// rely on disabling the per-repo *execution* hooks below.)
fn parse_dir_remote_github_slugs(dir: &Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(safe_git_config_flags())
        .args(["-C"])
        .arg(dir)
        .args(["remote", "-v"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        // Refuse host-level git config so a $GIT_CONFIG_GLOBAL
        // override (env injected by the user shell) can't sneak past
        // the `-c` overrides above. The empty values turn into a
        // non-existent path lookup, which git treats as missing.
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut slugs: Vec<String> = Vec::new();
    for line in text.lines() {
        // line format: "<name>\t<url> (<fetch|push>)"
        let url = match line.split_ascii_whitespace().nth(1) {
            Some(u) => u,
            None => continue,
        };
        if let Some(slug) = parse_github_slug(url) {
            if !slugs.iter().any(|s| s.eq_ignore_ascii_case(&slug)) {
                slugs.push(slug);
            }
        }
    }
    slugs
}

/// Parse `.gitmodules` (INI-style) via `git config -f` — using git
/// itself avoids reinventing an INI parser AND inherits the same
/// safe.directory / fsmonitor neutralization. `-f <path>` reads
/// ONLY that file (no chdir into the repo), so repo-local hooks
/// can't fire.
fn parse_gitmodules_github_slugs(dir: &Path) -> Vec<String> {
    let gitmodules = dir.join(".gitmodules");
    // `symlink_metadata` (vs `is_file`/`metadata`) deliberately does
    // NOT follow symlinks. A hostile checkout containing
    // `.gitmodules -> ~/.config/git/config` (or any out-of-tree path)
    // would otherwise route the parse at an unrelated host file —
    // and any stale `submodule.*.url` entries leaking out of it
    // would silently widen this launch's GitHub scope. The
    // legitimate `.gitmodules` is always a regular file.
    match std::fs::symlink_metadata(&gitmodules) {
        Ok(m) if m.is_file() => {}
        _ => return Vec::new(),
    }
    let out = std::process::Command::new("git")
        .args(safe_git_config_flags())
        .args(["config", "-f"])
        .arg(&gitmodules)
        .args(["--get-regexp", r"^submodule\..*\.url$"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut slugs: Vec<String> = Vec::new();
    for line in text.lines() {
        // line format: "submodule.<name>.url <url>"
        let url = match line.split_ascii_whitespace().nth(1) {
            Some(u) => u,
            None => continue,
        };
        if let Some(slug) = parse_github_slug(url) {
            slugs.push(slug);
        }
    }
    slugs
}

/// `-c` flags reused by every git invocation we make on
/// possibly-untrusted host paths. Keeps the hardening identical
/// across remote/submodule scans.
fn safe_git_config_flags() -> [&'static str; 12] {
    [
        // Disable repo-local config that runs binaries:
        "-c", "core.fsmonitor=",
        "-c", "core.fsmonitorHookVersion=",
        // Editor/pager fall back to cat — nothing we run here
        // needs them but a repo `core.pager = !bad-script` would
        // otherwise fire on output paging.
        "-c", "core.pager=cat",
        "-c", "core.editor=:",
        // Trust the dir even if owned by another UID (we only read).
        "-c", "safe.directory=*",
        // Don't fetch anything across this invocation.
        "-c", "protocol.allow=never",
    ]
}

/// Pull `owner/repo` from a GitHub remote URL. Returns `None` for
/// non-GitHub URLs. Strips a trailing `.git`. Handles both:
/// - `https://github.com/owner/repo[.git]`
/// - `git@github.com:owner/repo[.git]`
/// - `ssh://git@github.com/owner/repo[.git]`
fn parse_github_slug(url: &str) -> Option<String> {
    let rest = if let Some(r) = url.strip_prefix("https://github.com/") {
        r
    } else if let Some(r) = url.strip_prefix("http://github.com/") {
        r
    } else if let Some(r) = url.strip_prefix("git@github.com:") {
        r
    } else if let Some(r) = url.strip_prefix("ssh://git@github.com/") {
        r
    } else if let Some(r) = url.strip_prefix("ssh://git@github.com:") {
        // some hosts include a port-style colon; strip until next /
        r.split_once('/').map(|(_, p)| p)?
    } else {
        return None;
    };
    let trimmed = rest.trim_end_matches('/');
    let mut parts = trimmed.split('/');
    let owner = parts.next()?;
    let repo_raw = parts.next()?;
    if owner.is_empty() || repo_raw.is_empty() {
        return None;
    }
    // Strip exactly ONE trailing `.git` (the git URL convention).
    // `trim_end_matches` here would be greedy — `repo.git.git`
    // would round-trip as `repo` and miss the actual repo name.
    let repo = repo_raw.strip_suffix(".git").unwrap_or(repo_raw);
    if repo.is_empty() {
        return None;
    }
    // Reject path-traversal-shaped segments. A URL like
    // `https://github.com/../attacker/repo` would otherwise yield
    // the bogus slug `"../attacker"`, which the intercept hook's
    // path-traversal guard already drops — but it still pollutes
    // the `==> GitHub repo scope` summary and the `--allowed-repo`
    // argv with junk that masks malicious .gitmodules entries.
    if matches!(owner, "." | "..") || matches!(repo, "." | "..") {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// Single-quote `s` for use as a single argv element in a `bash -c`
/// line. Embedded single quotes are split out with the standard
/// `'\''` trick. Adequate for forwarding arbitrary user-supplied agent
/// args through the resolv.conf prelude wrapper.
fn shell_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_escape_handles_simple_and_quoted() {
        assert_eq!(shell_escape("foo"), "'foo'");
        assert_eq!(shell_escape("--flag=value with spaces"), "'--flag=value with spaces'");
        assert_eq!(shell_escape("don't"), "'don'\\''t'");
        assert_eq!(shell_escape(""), "''");
    }

    // ── parse_github_slug ────────────────────────────────────────

    #[test]
    fn parse_github_slug_https_with_and_without_dot_git() {
        assert_eq!(
            parse_github_slug("https://github.com/wirenboard/agent-vm.git"),
            Some("wirenboard/agent-vm".into())
        );
        assert_eq!(
            parse_github_slug("https://github.com/wirenboard/agent-vm"),
            Some("wirenboard/agent-vm".into())
        );
        // Extra path components beyond the repo are ignored.
        assert_eq!(
            parse_github_slug("https://github.com/wirenboard/agent-vm/tree/main"),
            Some("wirenboard/agent-vm".into())
        );
        // http also works.
        assert_eq!(
            parse_github_slug("http://github.com/o/r.git"),
            Some("o/r".into())
        );
    }

    #[test]
    fn parse_github_slug_scp_and_ssh_url_forms() {
        // scp-like: git@github.com:owner/repo[.git]
        assert_eq!(
            parse_github_slug("git@github.com:wirenboard/agent-vm.git"),
            Some("wirenboard/agent-vm".into())
        );
        assert_eq!(
            parse_github_slug("git@github.com:wirenboard/agent-vm"),
            Some("wirenboard/agent-vm".into())
        );
        // URL form with /
        assert_eq!(
            parse_github_slug("ssh://git@github.com/wirenboard/agent-vm.git"),
            Some("wirenboard/agent-vm".into())
        );
        // URL form with port: ssh://git@github.com:22/owner/repo
        assert_eq!(
            parse_github_slug("ssh://git@github.com:22/wirenboard/agent-vm"),
            Some("wirenboard/agent-vm".into())
        );
    }

    #[test]
    fn parse_github_slug_rejects_non_github_urls() {
        assert_eq!(parse_github_slug("https://gitlab.com/o/r"), None);
        assert_eq!(parse_github_slug("https://example.com/github.com/o/r"), None);
        assert_eq!(parse_github_slug(""), None);
        assert_eq!(parse_github_slug("not a url"), None);
    }

    #[test]
    fn parse_github_slug_handles_dot_git_only_once() {
        // Regression: `trim_end_matches(".git")` (greedy) would strip
        // both, yielding `o/repo` instead of `o/repo.git`. With
        // `strip_suffix` we strip exactly one.
        assert_eq!(
            parse_github_slug("https://github.com/o/repo.git.git"),
            Some("o/repo.git".into())
        );
    }

    #[test]
    fn parse_github_slug_rejects_empty_owner_or_repo() {
        assert_eq!(parse_github_slug("https://github.com/"), None);
        assert_eq!(parse_github_slug("https://github.com/owner"), None);
        assert_eq!(parse_github_slug("https://github.com/owner/"), None);
        assert_eq!(parse_github_slug("https://github.com//repo"), None);
        // Only `.git` after the owner means an empty repo segment.
        assert_eq!(parse_github_slug("https://github.com/owner/.git"), None);
    }

    #[test]
    fn parse_github_slug_rejects_dot_and_dotdot_segments() {
        // A submodule URL like `https://github.com/../attacker/repo`
        // is shaped like a path-traversal — must not yield a slug.
        assert_eq!(parse_github_slug("https://github.com/../attacker"), None);
        assert_eq!(parse_github_slug("https://github.com/owner/.."), None);
        assert_eq!(parse_github_slug("https://github.com/./repo"), None);
        assert_eq!(parse_github_slug("https://github.com/owner/."), None);
        assert_eq!(parse_github_slug("git@github.com:../attacker.git"), None);
    }

    // ── parse_extra_mounts ───────────────────────────────────────

    #[test]
    fn parse_extra_mounts_mirror_form() {
        // Mirror form: HOST alone → guest = host. Resolve against
        // cwd so the host path canonicalize succeeds in the test.
        // Use `/` which always exists.
        let parsed = parse_extra_mounts(&["/".into()]).expect("ok");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].host, std::path::Path::new("/"));
        assert_eq!(parsed[0].guest, std::path::Path::new("/"));
    }

    #[test]
    fn parse_extra_mounts_remap_form() {
        let parsed = parse_extra_mounts(&["/:/guest-mount".into()]).expect("ok");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].host, std::path::Path::new("/"));
        assert_eq!(parsed[0].guest, std::path::Path::new("/guest-mount"));
    }

    #[test]
    fn parse_extra_mounts_rejects_relative_paths() {
        assert!(parse_extra_mounts(&["relative-host".into()]).is_err());
        assert!(parse_extra_mounts(&["/abs-host:relative-guest".into()]).is_err());
        assert!(parse_extra_mounts(&["relative-host:/abs-guest".into()]).is_err());
    }

    #[test]
    fn parse_extra_mounts_rejects_empty_side() {
        assert!(parse_extra_mounts(&[":/guest".into()]).is_err());
        assert!(parse_extra_mounts(&["/host:".into()]).is_err());
        assert!(parse_extra_mounts(&["".into()]).is_err());
    }

    #[test]
    fn parse_extra_mounts_rejects_nonexistent_host_path_at_canonicalize() {
        // canonicalize fails on missing paths → Err propagates.
        let r = parse_extra_mounts(&["/this/path/does/not/exist/anywhere".into()]);
        assert!(r.is_err());
    }

    // ── parse_publish_args ───────────────────────────────────────

    #[test]
    fn parse_publish_args_two_part_defaults_to_loopback_tcp() {
        let p = parse_publish_args(&["8080:80".into()]).expect("ok");
        assert_eq!(p.len(), 1);
        assert_eq!(
            p[0].host_bind,
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        );
        assert_eq!(p[0].host_port, 8080);
        assert_eq!(p[0].guest_port, 80);
        assert_eq!(p[0].protocol, PublishProto::Tcp);
    }

    #[test]
    fn parse_publish_args_three_part_with_bind() {
        let p = parse_publish_args(&["0.0.0.0:5000:5000".into()]).expect("ok");
        assert_eq!(
            p[0].host_bind,
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
        );
        assert_eq!(p[0].host_port, 5000);
        assert_eq!(p[0].guest_port, 5000);
    }

    #[test]
    fn parse_publish_args_explicit_tcp_suffix() {
        let p = parse_publish_args(&["8080:80/tcp".into()]).expect("ok");
        assert_eq!(p[0].protocol, PublishProto::Tcp);
    }

    /// UDP must be REJECTED with a clear error. Previously the parser
    /// accepted it, the stderr banner said "Publishing …/udp", and
    /// `PortPublisher::spawn_listener_one` silently dropped it — user
    /// got a "successful" publish that actually had no listener.
    #[test]
    fn parse_publish_args_rejects_udp() {
        let err = parse_publish_args(&["53:53/udp".into()])
            .expect_err("UDP must be rejected until upstream supports it")
            .to_string();
        assert!(
            err.contains("UDP is not yet supported"),
            "error should mention UDP unsupported, got: {err}"
        );
    }

    /// IPv6 host bind must work via the docker-style `[ADDR]:p:p`
    /// bracket form. Previously the parser split the whole body on
    /// `:` and IPv6 was unreachable.
    #[test]
    fn parse_publish_args_ipv6_bracket_bind() {
        let p = parse_publish_args(&["[::1]:8080:80".into()]).expect("ok");
        assert_eq!(p[0].host_bind, std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST));
        assert_eq!(p[0].host_port, 8080);
        assert_eq!(p[0].guest_port, 80);
    }

    #[test]
    fn parse_publish_args_ipv6_bracket_wildcard() {
        let p = parse_publish_args(&["[::]:5000:5000".into()]).expect("ok");
        assert_eq!(
            p[0].host_bind,
            std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
        );
    }

    #[test]
    fn parse_publish_args_ipv6_bracket_missing_closer_is_rejected() {
        // `[::1` without `]:` should be a clear error, not a silent
        // misparse.
        assert!(parse_publish_args(&["[::1:8080:80".into()]).is_err());
    }

    // ── parse_allow_egress ───────────────────────────────────────

    #[test]
    fn parse_allow_egress_accepts_bare_ipv4() {
        let r = parse_allow_egress(&["10.100.1.75".into()]).expect("ok");
        assert_eq!(r.len(), 1);
        // Bare IPv4 → /32 single-host CIDR.
        assert_eq!(r[0].prefix(), 32);
        assert_eq!(r[0].network().to_string(), "10.100.1.75");
    }

    #[test]
    fn parse_allow_egress_accepts_ipv4_cidr() {
        let r = parse_allow_egress(&["10.100.1.0/24".into()]).expect("ok");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].prefix(), 24);
    }

    #[test]
    fn parse_allow_egress_accepts_ipv6_cidr() {
        let r = parse_allow_egress(&["fd00::/64".into()]).expect("ok");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].prefix(), 64);
    }

    #[test]
    fn parse_allow_egress_accepts_bare_ipv6() {
        let r = parse_allow_egress(&["fd00::1".into()]).expect("ok");
        assert_eq!(r[0].prefix(), 128);
    }

    #[test]
    fn parse_allow_egress_rejects_garbage() {
        assert!(parse_allow_egress(&["not-an-ip".into()]).is_err());
        assert!(parse_allow_egress(&["10.0.0.1/99".into()]).is_err());
        assert!(parse_allow_egress(&["".into()]).is_err());
    }

    #[test]
    fn parse_publish_args_rejects_bad_input() {
        assert!(parse_publish_args(&["80".into()]).is_err());
        assert!(parse_publish_args(&["a:b".into()]).is_err());
        assert!(parse_publish_args(&["0:80".into()]).is_err());
        assert!(parse_publish_args(&["80:0".into()]).is_err());
        assert!(parse_publish_args(&["999999:80".into()]).is_err());
        assert!(parse_publish_args(&["1:2:3:4".into()]).is_err());
    }

    // ── mkdir_chain ──────────────────────────────────────────────

    #[test]
    fn mkdir_chain_yields_path_prefixes() {
        let chain = mkdir_chain(std::path::Path::new("/home/user/proj"));
        assert_eq!(chain, vec!["/home", "/home/user", "/home/user/proj"]);
    }

    #[test]
    fn mkdir_chain_root_is_empty() {
        let chain = mkdir_chain(std::path::Path::new("/"));
        assert_eq!(chain, Vec::<String>::new());
    }

    #[test]
    fn mkdir_chain_single_segment() {
        let chain = mkdir_chain(std::path::Path::new("/workspace"));
        assert_eq!(chain, vec!["/workspace"]);
    }

    // ── guest_path_is_safe ───────────────────────────────────────

    #[test]
    fn guest_path_safe_for_normal_paths() {
        assert!(guest_path_is_safe(std::path::Path::new("/home/u/proj")));
        assert!(guest_path_is_safe(std::path::Path::new("/workspace")));
        assert!(guest_path_is_safe(std::path::Path::new("/opt/foo")));
    }

    #[test]
    fn guest_path_unsafe_under_tmpfs_prefixes() {
        // The guest tmpfs-mounts these at boot, wiping any bake-time
        // mount point — so we can't mirror them.
        assert!(!guest_path_is_safe(std::path::Path::new("/tmp")));
        assert!(!guest_path_is_safe(std::path::Path::new("/tmp/anything")));
        assert!(!guest_path_is_safe(std::path::Path::new("/run")));
        assert!(!guest_path_is_safe(std::path::Path::new("/run/user/1000")));
        assert!(!guest_path_is_safe(std::path::Path::new("/dev/shm")));
        assert!(!guest_path_is_safe(std::path::Path::new("/var/run/foo")));
    }

    #[test]
    fn guest_path_safe_for_lookalikes_outside_tmpfs() {
        // /tmpfoo is NOT under /tmp/ — must remain safe.
        assert!(guest_path_is_safe(std::path::Path::new("/tmpfoo")));
        assert!(guest_path_is_safe(std::path::Path::new("/run-extra")));
    }

    // ── detect_github_repos against the live worktree ─────────────
    //
    // Exercises the real `git` invocation on the worktree this test
    // is built in. Skips itself cleanly if the workspace doesn't
    // have a github origin (e.g. distro packagers building from a
    // tarball), so it stays useful in the dev tree without being
    // load-bearing for releases.

    fn workspace_root() -> std::path::PathBuf {
        // crates/agent-vm/ → workspace root is two levels up.
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("CARGO_MANIFEST_DIR has two parents")
            .to_path_buf()
    }

    #[test]
    fn detect_github_repos_includes_submodules() {
        let root = workspace_root();
        if !root.join(".gitmodules").is_file() {
            eprintln!("skipping: no .gitmodules at {root:?}");
            return;
        }
        let slugs = detect_github_repos(&root, std::iter::empty());
        // The rewrite worktree vendors microsandbox as a submodule.
        // If origin happens to be non-github (rare), we still expect
        // the submodule slug.
        assert!(
            slugs.iter().any(|s| s.eq_ignore_ascii_case("wirenboard/microsandbox")),
            "expected wirenboard/microsandbox in scope, got {slugs:?}"
        );
    }

    #[test]
    fn parse_gitmodules_returns_empty_when_file_missing() {
        let tmp = std::env::temp_dir().join(format!(
            "agent-vm-gitmodules-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let slugs = parse_gitmodules_github_slugs(&tmp);
        std::fs::remove_dir_all(&tmp).ok();
        assert!(slugs.is_empty(), "no .gitmodules → no slugs, got {slugs:?}");
    }
}

async fn notify_if_update_available(image: &str) {
    use crate::image_check::{UpdateState, check_for_update};
    match check_for_update(image).await {
        Ok(Some(UpdateState::UpdateAvailable { cached, remote })) => {
            eprintln!(
                "==> A newer image is available in the registry (cached {cached}, registry {remote})"
            );
            eprintln!(
                "==> Run `agent-vm pull` to fetch it. Continuing with the cached image."
            );
        }
        // UpToDate / NotCached: nothing to say.
        // None / Err: registry unreachable etc. — stay quiet.
        _ => {}
    }
}

