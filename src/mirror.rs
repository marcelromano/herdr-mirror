// Reconciliation: project a remote herdr server's workspaces/tabs/panes into
// the local server as `prefix:*` mirror objects, and push the remote's
// authoritative agent statuses onto the mirror panes.
//
// The id map (src/state.rs, persisted per host) distinguishes "user closed the
// mirror locally" (tombstone — don't recreate) from "remote object went away"
// (close the mirror).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::ApiClient;
use crate::config::HostConfig;
use crate::state::{load_state, save_state, HostState, PaneEntry, WsEntry};
use crate::util::{Logger, Result};

#[path = "orphan_gc.rs"]
mod orphan_gc;

/// `herdr-mirror gc-plan`: read-only report of what orphan GC would do. Never
/// writes to Herdr, never saves state, never contacts the daemon or remotes.
pub async fn cmd_gc_plan(env: crate::util::Env) -> Result<()> {
    let config = crate::config::load_config(&env.config_search)?;
    let local = ApiClient::connect(&env.local_socket).await?;
    for h in &config.hosts {
        let state = load_state(&env.state_dir, &h.name);
        for line in orphan_gc::plan(&local, &env.state_dir, &h.name, &h.target, &state).await {
            println!("{line}");
        }
    }
    Ok(())
}

// --- snapshot shapes (subset of the API's SessionSnapshot) ---

#[derive(Debug, Clone, Deserialize)]
pub struct WsInfo {
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    pub tab_count: Option<u64>,
    pub pane_count: Option<u64>,
    pub active_tab_id: Option<String>,
    /// custom metadata tokens the remote publishes. `default` on purpose: a
    /// pre-0.7.4 remote never sends this.
    #[serde(default)]
    pub tokens: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TabInfo {
    pub tab_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    /// unread today, but part of the pane wire shape — kept so the struct
    /// documents what the API actually returns
    #[allow(dead_code)]
    pub label: Option<String>,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
}

/// Agent fields as they appear both in snapshot `agents[]` and in
/// `pane.agent_status_changed` event data (null fields omitted there).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentInfo {
    #[serde(default)]
    pub pane_id: String,
    pub agent: Option<String>,
    pub display_agent: Option<String>,
    pub name: Option<String>,
    /// the pane's reported title (`agents[].title`, and what the
    /// pane_agent_status_changed event carries). Its own field, not an alias on
    /// `name`: the remote sets them independently, and an alias funnels both
    /// keys into one slot, a duplicate-field error that fails the whole
    /// snapshot parse, not just this row.
    #[serde(default)]
    pub title: Option<String>,
    /// the remote's live terminal title (e.g. a coding agent's current task
    /// summary), stripped of spinner/status glyphs. Only present on hosts new
    /// enough to publish it — default so older remotes still parse.
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    #[serde(default)]
    pub terminal_title: Option<String>,
    #[serde(default)]
    pub agent_status: Option<String>,
    pub custom_status: Option<String>,
    pub state_labels: Option<BTreeMap<String, String>>,
    /// custom metadata tokens the remote publishes ($model, $summary, …).
    /// `default` on purpose: a pre-0.7.4 remote never sends this.
    #[serde(default)]
    pub tokens: HashMap<String, String>,
}

impl AgentInfo {
    /// Does this describe a live agent (vs. a sparse release event)?
    pub fn has_agent(&self) -> bool {
        self.agent.as_deref().is_some_and(|a| !a.is_empty())
            || self.agent_status.as_deref().is_some_and(|s| s != "unknown")
    }

    /// The single `title` slot `pane.report_metadata` accepts. A remote agent
    /// with a user-given `name` keeps showing it (unchanged behavior — don't
    /// bury a name the user picked under an ever-changing task title). Only
    /// when there's no name do we fall back to the pane's reported title, then
    /// to the remote's live terminal title, so a mirrored agent's current task
    /// is visible instead of blank.
    pub fn effective_title(&self) -> Option<&str> {
        self.name
            .as_deref()
            .or(self.title.as_deref())
            .or(self.terminal_title_stripped.as_deref())
            .or(self.terminal_title.as_deref())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LayoutRect {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LayoutPaneSnapshot {
    pub pane_id: String,
    pub rect: LayoutRect,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LayoutSnapshot {
    #[allow(dead_code)]
    pub tab_id: String,
    #[serde(default)]
    pub panes: Vec<LayoutPaneSnapshot>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub workspaces: Vec<WsInfo>,
    #[serde(default)]
    pub tabs: Vec<TabInfo>,
    #[serde(default)]
    pub panes: Vec<PaneInfo>,
    #[serde(default)]
    pub agents: Vec<AgentInfo>,
    #[serde(default)]
    pub layouts: Vec<LayoutSnapshot>,
}

pub async fn fetch_snapshot(api: &ApiClient) -> Result<Snapshot> {
    #[derive(Deserialize)]
    struct Res {
        snapshot: Snapshot,
    }
    let res: Res = api.request_t("session.snapshot", json!({})).await?;
    Ok(res.snapshot)
}

// --- layout tree ---

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LayoutNode {
    Pane {
        pane_id: Option<String>,
        label: Option<String>,
    },
    Split {
        direction: String,
        ratio: f64,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

/// Locate `pane_id` in a split tree: the parent split's direction (already
/// "right"/"down", pane.split's vocabulary) plus the sibling subtree's pane ids
/// ordered nearest-to-the-split-point first. Fallback for a pane
/// `layout_sync::plan_placements` can't place faithfully; the shape-preserving
/// path lives there.
pub fn locate_in_layout(node: &LayoutNode, pane_id: &str) -> Option<(String, Vec<String>)> {
    let LayoutNode::Split {
        direction,
        first,
        second,
        ..
    } = node
    else {
        return None;
    };
    let is_the_pane =
        |n: &LayoutNode| matches!(n, LayoutNode::Pane { pane_id: Some(p), .. } if p == pane_id);
    if is_the_pane(first) {
        let mut sibs = Vec::new();
        walk_pane_ids(second, &mut sibs);
        return Some((direction.clone(), sibs));
    }
    if is_the_pane(second) {
        let mut sibs = Vec::new();
        walk_pane_ids(first, &mut sibs);
        sibs.reverse();
        return Some((direction.clone(), sibs));
    }
    locate_in_layout(first, pane_id).or_else(|| locate_in_layout(second, pane_id))
}

fn walk_pane_ids(node: &LayoutNode, out: &mut Vec<String>) {
    match node {
        LayoutNode::Pane { pane_id, .. } => out.push(pane_id.clone().unwrap_or_default()),
        LayoutNode::Split { first, second, .. } => {
            walk_pane_ids(first, out);
            walk_pane_ids(second, out);
        }
    }
}

/// Layout tree as plain shell panes (no `command`, so herdr won't set
/// `launch_argv` and treat them as agents); the streamer is exec'd in afterward.
fn map_node(node: &LayoutNode, cwd: &str) -> Value {
    match node {
        LayoutNode::Pane { pane_id: _, label } => json!({
            "type": "pane",
            "label": label,
            "cwd": cwd,
        }),
        LayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => json!({
            "type": "split",
            "direction": direction,
            "ratio": ratio,
            "first": map_node(first, cwd),
            "second": map_node(second, cwd),
        }),
    }
}

/// Drop panes whose mirror the user closed (tombstoned) from an exported
/// layout tree, collapsing a split left with one child. None means every pane
/// in the tree is tombstoned — the whole tab's mirror was closed.
fn prune_closed(node: &LayoutNode, panes: &BTreeMap<String, PaneEntry>) -> Option<LayoutNode> {
    match node {
        LayoutNode::Pane { pane_id, .. } => {
            let closed = pane_id
                .as_deref()
                .and_then(|rid| panes.get(rid))
                .is_some_and(|e| e.is_tombstoned());
            (!closed).then(|| node.clone())
        }
        LayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => match (prune_closed(first, panes), prune_closed(second, panes)) {
            (Some(f), Some(s)) => Some(LayoutNode::Split {
                direction: direction.clone(),
                ratio: *ratio,
                first: Box::new(f),
                second: Box::new(s),
            }),
            (one, two) => one.or(two),
        },
    }
}

/// Which mirrors `apply_hidden` would close, and what survives the map.
///
/// Split out so the gating is testable without a live herdr: the first version
/// of this closed every mirror on EVERY call because it never read the marker,
/// which turned the daemon into a destroy/rebuild loop. Full-green tests said
/// nothing, because nothing reached the function.
pub(crate) fn hidden_close_plan(
    hidden: bool,
    state: &mut HostState,
    live: &std::collections::HashSet<String>,
) -> Vec<String> {
    if !hidden {
        return Vec::new();
    }
    let doomed: Vec<String> = state
        .workspaces
        .values()
        .filter(|e| !e.is_tombstoned() && live.contains(&e.local_id))
        .map(|e| e.local_id.clone())
        .collect();
    if doomed.is_empty() {
        return doomed;
    }
    // tombstones on both maps survive: they mean "the user closed this, do not
    // recreate until `restore`", which hiding must not quietly forget
    state.workspaces.retain(|_, e| e.is_tombstoned());
    state.panes.retain(|_, e| e.is_tombstoned());
    state.tabs.clear();
    doomed
}

/// Take a hidden host's mirrors off the sidebar.
///
/// Lives in the daemon and needs only the LOCAL api, so it works while the
/// remote is unreachable — which is the main reason to hide a connection in the
/// first place (a dead host leaving reconnecting panes on screen). `converge`
/// cannot do this job: it only runs while connected, and it is also called by
/// one-shots whose close tracker nobody reads.
///
/// Tombstoned entries are kept. A tombstone means "the user closed this, do not
/// recreate it until `restore`", and hiding a host must not quietly forget that
/// — otherwise `show` resurrects every mirror they had deliberately closed.
///
/// Two layers against close-through, the same pair `teardown` uses: each id is
/// marked as ours before its close, and the map entries are dropped first so a
/// missed mark has nothing to attribute a close to.
pub async fn apply_hidden(
    local: &ApiClient,
    state_dir: &std::path::Path,
    host_name: &str,
    log: &Logger,
    closes: &crate::closes::Closes,
) {
    // The guard, not the caller's job: this is called on every host_task loop
    // and before every connected converge, so without it the daemon closes the
    // mirrors it just created, forever.
    let hidden = crate::state::is_hidden(state_dir, host_name);
    if !hidden {
        return;
    }
    let mut state = load_state(state_dir, host_name);
    // only ids herdr still shows. Note this filters ids that are GONE, not ids
    // that now belong to someone else: a local server restart can reassign one,
    // and this cannot tell. Converge's own close paths share that weakness.
    let live: std::collections::HashSet<String> = match fetch_snapshot(local).await {
        Ok(snap) => snap
            .workspaces
            .iter()
            .map(|w| w.workspace_id.clone())
            .collect(),
        Err(e) => {
            // the one path where hide legitimately does nothing; say so, or the
            // mirrors stay up with no explanation anywhere
            log.log(&format!(
                "hidden: local snapshot failed for {host_name}: {e}"
            ));
            return;
        }
    };
    let doomed = hidden_close_plan(hidden, &mut state, &live);
    if doomed.is_empty() {
        return;
    }
    if let Err(e) = save_state(state_dir, host_name, &state) {
        log.log(&format!(
            "hidden: could not save state for {host_name}: {e}"
        ));
        return;
    }
    for local_id in &doomed {
        log.log(&format!("hidden — closing mirror workspace {local_id}"));
        if let Ok(mut t) = closes.lock() {
            t.mark_self_close(local_id);
        }
        if let Err(e) = local
            .request("workspace.close", json!({ "workspace_id": local_id }))
            .await
        {
            log.log(&format!("hidden: close failed for {local_id}: {e}"));
        }
    }
}

/// Mark a local id the plugin itself is about to close, so the close event it
/// raises isn't read back as the user closing the mirror (see closes.rs).
fn mark_self_close(deps: &ConvergeDeps, local_id: &str) {
    if let Ok(mut t) = deps.closes.lock() {
        t.mark_self_close(local_id);
    }
}

fn map_status(remote: &str) -> &'static str {
    match remote {
        "working" => "working",
        "blocked" => "blocked",
        "idle" => "idle",
        // local herdr derives "done" from working→idle while unseen
        "done" => "idle",
        _ => "unknown",
    }
}

pub fn mirror_source(host_name: &str) -> String {
    format!("plugin:mirror:{host_name}")
}

/// The server rejects custom_status longer than this.
const CUSTOM_STATUS_MAX: usize = 32;

fn clamp_status(s: &str) -> String {
    s.chars().take(CUSTOM_STATUS_MAX).collect()
}

// Observe requests = the remote pane's real size + a margin that absorbs
// modest remote resizes (a larger resize clips until the wrapper reconnects).
const OBSERVE_MARGIN_COLS: u32 = 16;
const OBSERVE_MARGIN_ROWS: u32 = 8;

/// How to resolve a mirror label state (workspace or tab).
#[derive(Debug, PartialEq)]
enum LabelAction {
    /// labels agree — nothing to do
    InSync,
    /// user renamed the mirror locally → rename the REMOTE object to this
    PushRemote(String),
    /// remote is the authority (remote renamed, or unknown history) → restamp local
    RestampLocal,
}

/// Two-way rename resolution. `last_remote` is the remote label as of the
/// previous converge (None = pre-upgrade state file / first sight: remote wins).
///
/// `prefix` is `Some` for workspaces, whose mirrors carry the "<prefix>: " form,
/// and `None` for tabs, which carry the remote's label verbatim.
fn resolve_label(
    prefix: Option<&str>,
    remote_label: &str,
    local_label: &str,
    last_remote: Option<&str>,
) -> LabelAction {
    let expected = match prefix {
        Some(p) => format!("{p}: {remote_label}"),
        None => remote_label.to_string(),
    };
    if local_label == expected {
        return LabelAction::InSync;
    }
    if last_remote != Some(remote_label) {
        // remote changed since we last stamped (or no history) — remote wins
        return LabelAction::RestampLocal;
    }
    // remote unchanged, local differs → this is a user rename. Accept it with
    // or without the "<prefix>: " convention; empty/degenerate names restamp.
    let stripped = match prefix {
        Some(p) => local_label
            .strip_prefix(&format!("{p}: "))
            .unwrap_or(local_label)
            .trim(),
        None => local_label.trim(),
    };
    if stripped.is_empty() || stripped == remote_label {
        LabelAction::RestampLocal
    } else {
        LabelAction::PushRemote(stripped.to_string())
    }
}

pub struct ConvergeDeps {
    /// Only the daemon shares its self-close tracker with the event loop.
    pub orphan_gc: bool,
    pub local: ApiClient,
    pub remote: ApiClient,
    pub host: HostConfig,
    pub state_dir: PathBuf,
    pub log: Logger,
    /// mirror closing a workspace/pane locally onto the remote (see MirrorConfig)
    pub close_remote_on_local_close: bool,
    /// event-confirmed local closes. Absence from the local snapshot is
    /// ambiguous (rebuild in flight, failed converge, server restart), so only a
    /// close event that wasn't our own may close the remote.
    pub closes: crate::closes::Closes,
}

/// argv for one mirror pane: this same binary in `pane` mode. Panes without a
/// known size get no --cols/--rows (the wrapper falls back to a default).
pub(crate) fn cmd_for_pane(
    host: &HostConfig,
    state_dir: &std::path::Path,
    sizes: &HashMap<String, LayoutRect>,
) -> impl Fn(&str) -> Vec<String> {
    let exe = crate::util::self_exe();
    let target = host.target.clone();
    let remote_bin = host.remote_bin.clone();
    let session = host.session.clone();
    let always_control = host.always_control;
    let max_cols = host.max_cols;
    let max_rows = host.max_rows;
    let kind = host.kind.clone();
    let docker_bin = host.docker_bin.clone();
    // daemon's ControlMaster socket for this host (see remote.rs); the streamer
    // reuses it for cheap foreground polls
    let ctl_path = crate::remote::control_path(state_dir, &host.name)
        .display()
        .to_string();
    let sizes = sizes.clone();
    move |pane_id: &str| {
        let mut argv = vec![
            exe.clone(),
            "pane".into(),
            target.clone(),
            pane_id.to_string(),
        ];
        // omit --remote-bin when auto (PATH then ~/.local/bin/herdr); pane
        // defaults to the same resolution so the argv stays short. A Windows
        // host has no such default — the path is required config and the pane
        // cannot rebuild it — so it always travels.
        if let Some(bin) = &remote_bin {
            argv.extend(["--remote-bin".into(), bin.clone()]);
        }
        if let Some(session) = &session {
            argv.extend(["--session".into(), session.clone()]);
        }
        if always_control {
            argv.push("--always-control".into());
        }
        // absent when uncapped, so the argv of an unconfigured host is unchanged
        if let Some(c) = max_cols {
            argv.extend(["--max-cols".into(), c.to_string()]);
        }
        if let Some(r) = max_rows {
            argv.extend(["--max-rows".into(), r.to_string()]);
        }
        // ssh only: the pane reuses the daemon's ControlMaster for cheap
        // foreground polls. Docker has no ControlMaster, and healing no longer
        // needs a host-identity token in the argv at all — it asks herdr what
        // is running in each pane instead (see daemon::has_live_streamer).
        match &kind {
            crate::config::HostKind::Ssh => {
                argv.extend(["--ctl-path".into(), ctl_path.clone()]);
            }
            crate::config::HostKind::Windows => {
                // No --ctl-path: a Windows host is never multiplexed (see
                // RemoteHost::uses_control_master), so the foreground poll opens
                // its own connection like every other call to this host.
                argv.push("--windows-host".into());
            }
            crate::config::HostKind::DockerContainer(name) => {
                argv.extend(["--container".into(), name.clone()]);
                argv.extend(["--docker-bin".into(), docker_bin.clone()]);
            }
            crate::config::HostKind::DockerFolder(folder) => {
                argv.extend(["--container-folder".into(), folder.clone()]);
                argv.extend(["--docker-bin".into(), docker_bin.clone()]);
            }
        }
        if let Some(rect) = sizes.get(pane_id) {
            argv.extend([
                "--cols".into(),
                (rect.width + OBSERVE_MARGIN_COLS).to_string(),
                "--rows".into(),
                (rect.height + OBSERVE_MARGIN_ROWS).to_string(),
            ]);
        }
        argv
    }
}

/// single-quote for a POSIX shell command line
#[cfg(not(windows))]
#[cfg_attr(windows, allow(dead_code))]
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// PowerShell uses single quotes too, but escapes an embedded quote by
/// doubling it. Git Bash and other POSIX shells accept this form as well.
#[cfg(windows)]
#[cfg_attr(windows, allow(dead_code))]
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Reconcile one mirrored tab's geometry: exchange panes into the arrangement
/// the remote has, then merge every split ratio three ways so a resize
/// propagates in whichever direction it was actually made.
///
/// `always_control` decides ties. It already means "this daemon drives the
/// remote's pane sizes" (the remote is headless, the local window is the only
/// one anyone looks at), so the local side wins there; a watch-only host has its
/// own display and its layout is authoritative.
async fn reconcile_tab_geometry(
    deps: &ConvergeDeps,
    state: &mut HostState,
    remote_tab: &str,
    local_tab: &str,
    remote_panes: &[&PaneInfo],
) {
    #[derive(Deserialize)]
    struct Exported {
        layout: ExportedLayout,
    }
    #[derive(Deserialize)]
    struct ExportedLayout {
        root: LayoutNode,
    }
    let remote_layout = deps
        .remote
        .request_t::<Exported>("layout.export", json!({ "tab_id": remote_tab }))
        .await;
    let local_layout = deps
        .local
        .request_t::<Exported>("layout.export", json!({ "tab_id": local_tab }))
        .await;
    let (Ok(remote), Ok(local)) = (remote_layout, local_layout) else {
        return;
    };

    let map: BTreeMap<String, String> = remote_panes
        .iter()
        .filter_map(|p| {
            state
                .panes
                .get(&p.pane_id)
                .filter(|e| !e.is_tombstoned())
                .map(|e| (p.pane_id.clone(), e.local_id.clone()))
        })
        .collect();
    let prefix = format!("{remote_tab}|");
    let base: BTreeMap<String, f64> = state
        .ratios
        .iter()
        .filter_map(|(k, v)| k.strip_prefix(&prefix).map(|path| (path.to_string(), *v)))
        .collect();
    let plan = crate::layout_sync::plan_sync(
        &remote.layout.root,
        &local.layout.root,
        &map,
        &base,
        deps.host.always_control,
    );

    if plan.structural_mismatch {
        // Shapes disagree, so ratios aren't comparable. Forget the agreement so
        // that whenever the shapes line up again the remote's geometry is
        // adopted cleanly, and say so once — the base is empty on later passes,
        // so this logs on the pass where it diverges, not on every one after.
        if !base.is_empty() {
            log_geometry_drift(&deps.log, remote_tab);
        }
        state.ratios.retain(|k, _| !k.starts_with(&prefix));
        return;
    }

    // swaps first: a ratio describes a position, so it only means the right
    // thing once the right pane is sitting in it
    for (source, target) in &plan.swaps {
        if let Err(e) = deps
            .local
            .request(
                "pane.swap",
                json!({ "source_pane_id": source, "target_pane_id": target }),
            )
            .await
        {
            deps.log.log(&format!(
                "{remote_tab}: pane swap failed ({e}) — retrying next pass"
            ));
            return;
        }
    }

    let mut failed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for fix in &plan.ratios {
        let (api, tab) = match fix.apply_to {
            crate::layout_sync::Side::Local => (&deps.local, local_tab),
            crate::layout_sync::Side::Remote => (&deps.remote, remote_tab),
        };
        let params = json!({ "tab_id": tab, "path": fix.path, "ratio": fix.ratio });
        if let Err(e) = api.request("layout.set_split_ratio", params).await {
            deps.log
                .log(&format!("{remote_tab}: split ratio sync failed: {e}"));
            failed.insert(crate::layout_sync::path_key(&fix.path));
        }
    }
    // Only remember what actually landed. Recording an agreement we failed to
    // write would make the next pass read the difference as an edit on the
    // other side and push it the wrong way.
    for (path, ratio) in plan.base {
        if failed.contains(&path) {
            continue;
        }
        state.ratios.insert(format!("{prefix}{path}"), ratio);
    }
}

fn log_geometry_drift(log: &Logger, remote_tab: &str) {
    log.log(&format!(
        "{remote_tab}: mirror no longer matches the remote's split shape — sizes won't track until it does"
    ));
}

/// Split `target` and return the new local pane id. `ratio` is the remote
/// split's own ratio when the placement is faithful, and None when we're
/// falling back, so herdr's default stands rather than a ratio describing a
/// different split.
async fn split_mirror_pane(
    local: &ApiClient,
    target: &str,
    direction: &str,
    ratio: Option<f64>,
    cwd: &str,
    rid: &str,
) -> Result<String> {
    #[derive(Deserialize)]
    struct Split {
        pane: SplitPane,
    }
    #[derive(Deserialize)]
    struct SplitPane {
        pane_id: String,
    }
    let mut params = json!({
        "target_pane_id": target,
        "direction": direction,
        "cwd": cwd,
        "focus": false,
        "env": { RID_ENV: rid },
    });
    if let Some(r) = ratio {
        params["ratio"] = json!(r);
    }
    let split: Split = local.request_t("pane.split", params).await?;
    Ok(split.pane.pane_id)
}

/// Env var set on a mirror copy's shell (informational only: the API cannot
/// read it back; the durable marker is the streamer's argv, see orphan_gc).
pub(crate) const RID_ENV: &str = "HERDR_MIRROR_RID";

/// Exec the streamer into an already-created plain pane. Not `agent.start` (or a
/// layout `command`), which set `launch_argv` and would surface every mirror pane
/// as an agent row; a shell `exec` keeps it non-agent until a real agent is
/// reported onto it.
pub(crate) async fn spawn_streamer_pane(
    local: &ApiClient,
    state_dir: &std::path::Path,
    local_pane_id: &str,
    argv: &[String],
    log: &Logger,
) {
    // Automatic Windows streamer launch stays disabled until the daemon path has
    // a live lifecycle test; direct `pane` use is the supported attended path.
    #[cfg(windows)]
    {
        let _ = (local, state_dir, local_pane_id, argv);
        log.log("Windows streamer launch is disabled; use an attended pane");
    }

    #[cfg(not(windows))]
    {
        let (Some(ssh_target), Some(pane_target)) = (argv.get(2).cloned(), argv.get(3).cloned())
        else {
            log.log(&format!(
                "refusing malformed streamer command for {local_pane_id}"
            ));
            return;
        };
        match crate::util::claim_streamer_spawn(state_dir, &ssh_target, &pane_target, local_pane_id)
        {
            Ok(crate::util::StreamerSpawnClaim::Claimed) => {}
            Ok(crate::util::StreamerSpawnClaim::Active) => {
                log.log(&format!(
                    "streamer for {pane_target} already active in {local_pane_id}; not retyping"
                ));
                return;
            }
            Ok(crate::util::StreamerSpawnClaim::Pending) => {
                log.log(&format!("streamer launch for {pane_target} already pending in {local_pane_id}; not retyping"));
                return;
            }
            Err(e) => {
                log.log(&format!(
                    "cannot claim streamer launch for {local_pane_id}: {e}; not retyping"
                ));
                return;
            }
        }

        let line = format!(
            "exec {}\n",
            argv.iter()
                .map(|a| sh_quote(a))
                .collect::<Vec<_>>()
                .join(" ")
        );
        if let Err(e) = local
            .request(
                "pane.send_text",
                json!({ "pane_id": local_pane_id, "text": line }),
            )
            .await
        {
            crate::util::clear_streamer_spawn_pending(state_dir, local_pane_id);
            log.log(&format!("spawn streamer {local_pane_id}: {e}"));
            return;
        }

        // Typed input can be eaten by interactive shell startup (oh-my-zsh's
        // update prompt swallows the first key — in EVERY new shell until it's
        // answered). Verify the streamer registered its pidfile and retype the
        // exec if not, off-loop so a slow shell never stalls reconcile. The
        // alive-check right before each resend keeps a late-starting streamer
        // from getting the line typed into its stdin (which would forward it to
        // the remote pane as text).
        //
        // A Windows host needs a wider first window. Every exec there goes
        // through a ForceCommand wrapper that starts powershell.exe before
        // herdr.exe: measured at 1.04-1.16s per call, against roughly 0.3s for
        // a plain Unix host. The streamer takes more than one step before it
        // registers, so the Unix schedule expires while a perfectly healthy
        // launch is still in progress — and the retype then lands in the
        // stream's stdin and reaches the remote pane as typed text.
        let windows_host = argv.iter().any(|a| a == "--windows-host");
        let schedule: &[u64] = if windows_host {
            &[6000, 6000]
        } else {
            &[3000, 4000]
        };
        let (local, log, state_dir) = (local.clone(), log.clone(), state_dir.to_path_buf());
        let pane_id = local_pane_id.to_string();
        tokio::spawn(async move {
            for &wait_ms in schedule {
                tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                if crate::util::streamer_alive(&state_dir, &ssh_target, &pane_target)
                    || crate::util::pane_streamer_alive(&state_dir, &pane_id)
                {
                    return;
                }
                log.log(&format!(
                "streamer for {pane_target} not up in {pane_id} — shell startup likely ate the exec; retyping"
            ));
                if local
                    .request(
                        "pane.send_text",
                        json!({ "pane_id": pane_id, "text": line }),
                    )
                    .await
                    .is_err()
                {
                    crate::util::clear_streamer_spawn_pending(&state_dir, &pane_id);
                    return; // pane gone (closed meanwhile) — nothing to heal
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(4000)).await;
            if !crate::util::streamer_alive(&state_dir, &ssh_target, &pane_target)
                && !crate::util::pane_streamer_alive(&state_dir, &pane_id)
            {
                crate::util::clear_streamer_spawn_pending(&state_dir, &pane_id);
                log.log(&format!(
                "streamer for {pane_target} still not up in {pane_id} after retries — pane left as a shell"
            ));
            }
        });
    }
}

/// cwd every mirror pane runs in, doubling as the loop-guard marker: it's set at
/// pane creation so it's in the snapshot immediately (no exec race), and its name
/// can't collide with a real dir.
const MIRROR_CWD_MARKER: &str = ".mirror-pane";

fn mirror_pane_cwd(state_dir: &std::path::Path) -> std::path::PathBuf {
    state_dir.join(MIRROR_CWD_MARKER)
}

/// Is this remote pane another herdr-mirror's streamer pane? Read from the
/// snapshot cwd marker — free, and race-free.
fn pane_is_mirror(p: &PaneInfo) -> bool {
    let is_marker = |c: &Option<String>| {
        c.as_deref()
            .and_then(|s| std::path::Path::new(s).file_name())
            .and_then(|f| f.to_str())
            == Some(MIRROR_CWD_MARKER)
    };
    is_marker(&p.foreground_cwd) || is_marker(&p.cwd)
}

/// What to do with the local copy of a remote pane that went away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PaneReap {
    /// close exactly this local id (the one stored in the map)
    Close(String),
    /// nothing to close (tombstoned, or the copy is already gone locally)
    Absent,
    /// refuse and log: unsafe or ambiguous to close
    Refuse(&'static str),
}

/// remote pane id → local pane ids whose streamer process is showing it
/// (identified by the live streamer argv, not by any stored id).
pub(crate) type StreamerKeys = HashMap<String, Vec<String>>;

/// `(ssh target, remote pane id)` of a streamer argv
/// (`herdr-mirror pane <target> <rid> ...`), if it is one.
pub(crate) fn streamer_key(argv: &[Value]) -> Option<(String, String)> {
    let s = |i: usize| argv.get(i).and_then(|v| v.as_str());
    if !s(0)?.ends_with("herdr-mirror") || s(1)? != "pane" {
        return None;
    }
    Some((s(2)?.to_string(), s(3)?.to_string()))
}

/// Ask herdr (`pane.process_info`, the same source heal uses) what each local
/// mirror-marked pane is streaming, and index it by remote pane id for this
/// host's `target`. The streamer argv is the one stable link between a copy
/// and its remote pane that survives the local id changing.
pub(crate) async fn collect_streamer_keys(local: &ApiClient, snap: &Snapshot, target: &str) -> StreamerKeys {
    let mut keys = StreamerKeys::new();
    for p in snap.panes.iter().filter(|p| pane_is_mirror(p)) {
        let Ok(v) = local.request("pane.process_info", json!({ "pane_id": p.pane_id })).await else { continue };
        let Some(procs) = v.pointer("/process_info/foreground_processes").and_then(|x| x.as_array()) else {
            continue;
        };
        for pr in procs {
            let Some(argv) = pr.get("argv").and_then(|a| a.as_array()) else { continue };
            if let Some((t, rid)) = streamer_key(argv) {
                if t == target {
                    let e = keys.entry(rid).or_default();
                    if !e.contains(&p.pane_id) {
                        e.push(p.pane_id.clone());
                    }
                }
            }
        }
    }
    keys
}

fn foreign_agent_on(local: &Snapshot, pane: &PaneInfo) -> bool {
    !pane_is_mirror(pane) && local.agents.iter().any(|a| a.pane_id == pane.pane_id && a.has_agent())
}

/// Decide whether the copy of remote pane `rid` may be closed. The copy is
/// found by its streamer key first (`keys`), and by the id stored in the map
/// only when no pane streams `rid`. Exactly one keyed pane → close it (the
/// stored id may be stale); several → refuse; none → the stored id, if it is
/// still a pane. Pure, so the guards are testable without a live herdr.
pub(crate) fn plan_pane_reap(rid: &str, state: &HostState, local: &Snapshot, keys: &StreamerKeys) -> PaneReap {
    let Some(entry) = state.panes.get(rid) else { return PaneReap::Absent };
    if entry.is_tombstoned() {
        return PaneReap::Absent;
    }
    let keyed = keys.get(rid).map(|v| v.as_slice()).unwrap_or(&[]);
    let id: &str = match keyed {
        [] => entry.local_id.as_str(),
        [one] => one.as_str(),
        _ => return PaneReap::Refuse("several mirror panes stream this remote pane"),
    };
    if id.is_empty() {
        return PaneReap::Refuse("empty local id in map");
    }
    if state.panes.iter().any(|(k, e)| k != rid && e.local_id == id) {
        return PaneReap::Refuse("local id shared by several map entries");
    }
    let mut matching = local.panes.iter().filter(|p| p.pane_id == id);
    let Some(pane) = matching.next() else { return PaneReap::Absent };
    if matching.next().is_some() {
        return PaneReap::Refuse("local id ambiguous in snapshot");
    }
    // herdr reuses freed ids: a live agent on a pane that is not one of our
    // streamer panes is somebody else's session, never ours to close
    if foreign_agent_on(local, pane) {
        return PaneReap::Refuse("pane has a live agent and is not a mirror copy");
    }
    PaneReap::Close(id.to_string())
}

/// Mirror panes streaming a remote pane that is neither mapped nor present on
/// the remote. Returns `(close_now, seen_now)`: only ids that were already in
/// `prev_seen` are closed, so a copy mid-creation (streamer up before the map
/// is saved) is never reaped on first sight. `seen_now` replaces `prev_seen`.
pub(crate) fn plan_orphan_sweep(
    state: &HostState,
    local: &Snapshot,
    remote_pane_ids: &HashSet<&str>,
    keys: &StreamerKeys,
    prev_seen: &std::collections::BTreeSet<String>,
) -> (Vec<(String, String)>, std::collections::BTreeSet<String>) {
    let mapped_local: HashSet<&str> = state.panes.values().map(|e| e.local_id.as_str()).collect();
    let mut close = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (rid, ids) in keys {
        if state.panes.contains_key(rid) || remote_pane_ids.contains(rid.as_str()) {
            continue;
        }
        for id in ids {
            let Some(pane) = local.panes.iter().find(|p| &p.pane_id == id) else { continue };
            if mapped_local.contains(id.as_str()) || foreign_agent_on(local, pane) || !pane_is_mirror(pane) {
                continue;
            }
            if prev_seen.contains(id) {
                close.push((rid.clone(), id.clone()));
            } else {
                seen.insert(id.clone());
            }
        }
    }
    (close, seen)
}

/// Carry out a `PaneReap`. Returns true when the map entry may be dropped:
/// closed, already gone, or refused (logged, not retried forever). False only
/// on a transient close failure, so the next pass retries.
pub(crate) async fn execute_pane_reap(
    local: &ApiClient,
    log: &Logger,
    rid: &str,
    stored_id: &str,
    plan: PaneReap,
    mark: impl FnOnce(&str),
) -> bool {
    match plan {
        PaneReap::Absent => true,
        PaneReap::Refuse(why) => {
            log.log(&format!("remote pane {rid} gone — NOT closing mirror {stored_id:?}: {why}"));
            true
        }
        PaneReap::Close(id) => {
            log.log(&format!("remote pane {rid} gone — closing mirror {id}"));
            mark(&id);
            match local.request("pane.close", json!({ "pane_id": id })).await {
                Ok(_) => true,
                Err(e) if e.to_string().contains("not found") => true,
                Err(e) => {
                    log.log(&format!("close of mirror {id} for {rid} failed: {e} — will retry"));
                    false
                }
            }
        }
    }
}

// --- the converge pass ---

/// A fresh local id just entered the map (mirror created or adopted). Two
/// duties, both time-sensitive. Purge any stale user-close recorded against
/// the id: herdr reuses freed ids, so a close noted before this moment can
/// only refer to a previous holder, and letting it linger would close-through
/// the new mirror's REMOTE within USER_CLOSE_TTL. And persist the map right
/// away instead of at pass end, so the intercept hook's map read (250ms after
/// pane.created) sees the daemon-built object as mapped instead of judging it
/// native junk and closing it.
fn note_mapped(deps: &ConvergeDeps, state: &HostState, fresh_local_ids: &[String]) {
    if let Ok(mut t) = deps.closes.lock() {
        for id in fresh_local_ids {
            t.forget(id);
        }
    }
    if let Err(e) = save_state(&deps.state_dir, &deps.host.name, state) {
        // Not cosmetic: this write is what tells the intercept hook these
        // objects are ours. An unwritable state dir would otherwise leave every
        // daemon-created pane looking like native junk, silently.
        deps.log.log(&format!(
            "could not persist map after mapping fresh ids: {e}"
        ));
    }
}

/// Read-only hints, never authorization to close a pane. Counts include panes,
/// with their tab IDs, because a tab can contain both mapped and unmapped panes.
pub(crate) fn orphan_status(
    snap: &Snapshot,
    state: &HostState,
    alive: impl Fn(&str) -> bool,
) -> String {
    let workspaces: HashSet<&str> = state
        .workspaces
        .values()
        .map(|w| w.local_id.as_str())
        .collect();
    let mapped: HashSet<&str> = state.panes.values().map(|p| p.local_id.as_str()).collect();
    let tabs: HashSet<&str> = state.tabs.values().map(|t| t.local_id.as_str()).collect();
    let mut suspects = Vec::new();
    let mut protected = Vec::new();
    for p in &snap.panes {
        if !workspaces.contains(p.workspace_id.as_str())
            || mapped.contains(p.pane_id.as_str())
            || alive(&p.pane_id)
        {
            continue;
        }
        let mut reasons = Vec::new();
        if snap
            .panes
            .iter()
            .filter(|q| q.workspace_id == p.workspace_id)
            .count()
            <= 1
        {
            reasons.push("last workspace pane");
        }
        if tabs.contains(p.tab_id.as_str())
            && snap.panes.iter().filter(|q| q.tab_id == p.tab_id).count() <= 1
        {
            reasons.push("last mapped-tab pane");
        }
        if snap
            .agents
            .iter()
            .any(|a| a.pane_id == p.pane_id && a.has_agent())
        {
            reasons.push("agent");
        }
        let marker = |cwd: &Option<String>| {
            cwd.as_deref().is_some_and(|c| {
                c.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next()
                    == Some(MIRROR_CWD_MARKER)
            })
        };
        if !marker(&p.cwd) || !marker(&p.foreground_cwd) {
            reasons.push("adopted/unknown cwd");
        }
        let id = format!("{} (tab {})", p.pane_id, p.tab_id);
        if reasons.is_empty() {
            suspects.push(id);
        } else {
            protected.push(format!("{id}: {}", reasons.join(", ")));
        }
    }
    format!("  orphan shell suspects: {} [{}]; protected: {} [{}] (read-only hints; no collection promised)", suspects.len(), suspects.join("; "), protected.len(), protected.join("; "))
}

fn converge_lock_path(state_dir: &std::path::Path, host_name: &str) -> PathBuf {
    state_dir.join(format!("{host_name}.converge.lock"))
}

/// Try once to take the per-host converge lock. `Ok(None)` = another process
/// holds it.
pub(crate) fn try_converge_lock(
    state_dir: &std::path::Path,
    host_name: &str,
) -> std::io::Result<Option<std::fs::File>> {
    std::fs::create_dir_all(state_dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(converge_lock_path(state_dir, host_name))?;
    match crate::platform::try_lock(&file) {
        Ok(()) => Ok(Some(file)),
        // Windows reports contention as ERROR_LOCK_VIOLATION, not WouldBlock
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

const CONVERGE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Returns the post-converge state so callers don't re-read the state file.
///
/// Serialized per host across processes. The daemon and the `once` timer used
/// to converge the same host concurrently: both loaded the map, both saw a new
/// remote pane as unmapped, both created a local pane for it, and the last
/// `save_state` won. The loser's pane was never mapped (so never closed when
/// the remote pane went away) and its streamer claim found the other one
/// already streaming, so it was left as a bare shell — the orphan tabs.
pub async fn converge(deps: &ConvergeDeps) -> Result<HostState> {
    let deadline = std::time::Instant::now() + CONVERGE_LOCK_WAIT;
    let _lock = loop {
        match try_converge_lock(&deps.state_dir, &deps.host.name) {
            Ok(Some(f)) => break Some(f),
            Ok(None) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Ok(None) => {
                deps.log.log(&format!(
                    "[{}] another converge still holds the lock — skipping this pass",
                    deps.host.name
                ));
                return Ok(load_state(&deps.state_dir, &deps.host.name));
            }
            Err(e) => {
                deps.log.log(&format!(
                    "[{}] converge lock unavailable at {} ({e}) — skipping this pass",
                    deps.host.name,
                    converge_lock_path(&deps.state_dir, &deps.host.name).display()
                ));
                return Ok(load_state(&deps.state_dir, &deps.host.name));
            }
        }
    };
    // loaded only once the lock is held, so a pass never works from a map
    // another process is about to overwrite
    let mut state = load_state(&deps.state_dir, &deps.host.name);
    let result = converge_inner(deps, &mut state).await;
    if result.is_err() {
        state.orphan_candidates.clear();
    }
    // save even on error: a crash mid-pass must not orphan created mirrors
    save_state(&deps.state_dir, &deps.host.name, &state)?;
    result.map(|()| state)
}

async fn converge_inner(deps: &ConvergeDeps, state: &mut HostState) -> Result<()> {
    let host = &deps.host;
    let log = &deps.log;
    // Hidden hosts freeze here and go no further. Deliberately BEFORE the
    // snapshots: a hidden host should be quiet, not keep paying for two RPCs a
    // minute forever.
    //
    // This branch only freezes; it never closes. Closing has to happen exactly
    // once, in the process that owns the authoritative close tracker, and
    // `converge` is called by `once` and other one-shots that carry a throwaway
    // tracker (see `cmd_once`). Doing the close here meant those marked on a
    // tracker nobody reads while the daemon's event stream recorded every close
    // as user intent — which is the chain that closed two real remote
    // workspaces. The daemon does it in `apply_hidden` instead.
    if crate::state::is_hidden(&deps.state_dir, &deps.host.name) {
        return Ok(());
    }

    let (remote_snap, local_snap) =
        tokio::try_join!(fetch_snapshot(&deps.remote), fetch_snapshot(&deps.local))?;

    let mut local_ws_ids: HashSet<String> = local_snap
        .workspaces
        .iter()
        .map(|w| w.workspace_id.clone())
        .collect();
    let local_tab_ids: HashSet<&str> = local_snap.tabs.iter().map(|t| t.tab_id.as_str()).collect();
    let local_pane_ids: HashSet<&str> = local_snap
        .panes
        .iter()
        .map(|p| p.pane_id.as_str())
        .collect();
    let remote_ws_ids: HashSet<&str> = remote_snap
        .workspaces
        .iter()
        .map(|w| w.workspace_id.as_str())
        .collect();
    let remote_tab_ids: HashSet<&str> =
        remote_snap.tabs.iter().map(|t| t.tab_id.as_str()).collect();
    let remote_pane_ids: HashSet<&str> = remote_snap
        .panes
        .iter()
        .map(|p| p.pane_id.as_str())
        .collect();
    let mut sizes: HashMap<String, LayoutRect> = HashMap::new();
    for layout in &remote_snap.layouts {
        for p in &layout.panes {
            sizes.insert(p.pane_id.clone(), p.rect.clone());
        }
    }
    let cmd_for = cmd_for_pane(&deps.host, &deps.state_dir, &sizes);
    let _ = std::fs::create_dir_all(mirror_pane_cwd(&deps.state_dir));

    // 1. detect mirrors that are gone locally. Always tombstone (never remove)
    //    so this pass can't recreate them (the snapshot still lists the object);
    //    section 2 reaps the tombstoned entry once the remote is gone.
    //
    //    Closing the REMOTE is destructive, so it is driven only by an
    //    event-confirmed user close (see closes.rs) — never by absence alone,
    //    which also happens mid-rebuild, after a failed converge, or while the
    //    local server is restarting.
    let close_remote = deps.close_remote_on_local_close;
    let mine: HashSet<String> = state
        .workspaces
        .values()
        .map(|e| e.local_id.clone())
        .chain(state.panes.values().map(|e| e.local_id.clone()))
        .chain(state.tabs.values().map(|e| e.local_id.clone()))
        .collect();
    let user_closed = match deps.closes.lock() {
        Ok(mut t) => t.take_user_closed(&mine),
        Err(_) => HashSet::new(),
    };
    let mut ws_close_remote: Vec<String> = Vec::new();
    for (rid, entry) in state.workspaces.iter_mut() {
        if !entry.is_tombstoned()
            && !local_ws_ids.contains(&entry.local_id)
            && remote_ws_ids.contains(rid.as_str())
        {
            entry.tombstone = Some(true);
            if close_remote && user_closed.contains(&entry.local_id) {
                ws_close_remote.push(rid.clone());
            } else {
                log.log(&format!(
                    "workspace mirror for {rid} was closed locally — tombstoning"
                ));
            }
        }
    }
    for rid in &ws_close_remote {
        log.log(&format!(
            "workspace mirror for {rid} closed locally — closing remote workspace"
        ));
        if let Err(e) = deps
            .remote
            .request("workspace.close", json!({ "workspace_id": rid }))
            .await
        {
            log.log(&format!("remote workspace close failed for {rid}: {e}"));
        }
    }
    let pane_ws: HashMap<&str, &str> = remote_snap
        .panes
        .iter()
        .map(|p| (p.pane_id.as_str(), p.workspace_id.as_str()))
        .collect();
    let pane_tab: HashMap<&str, &str> = remote_snap
        .panes
        .iter()
        .map(|p| (p.pane_id.as_str(), p.tab_id.as_str()))
        .collect();
    let mut drop_panes: Vec<String> = Vec::new();
    let mut pane_close_remote: Vec<String> = Vec::new();
    for (rid, entry) in state.panes.iter_mut() {
        if !entry.is_tombstoned()
            && !local_pane_ids.contains(entry.local_id.as_str())
            && remote_pane_ids.contains(rid.as_str())
        {
            let ws_entry = pane_ws
                .get(rid.as_str())
                .and_then(|ws| state.workspaces.get(*ws));
            // if the pane's whole mirror workspace is gone, the stale pane
            // entry is collateral — drop it (its tombstoned workspace already
            // blocks recreation)
            match ws_entry {
                Some(w) if !w.is_tombstoned() && local_ws_ids.contains(&w.local_id) => {
                    entry.tombstone = Some(true);
                    // user intent covers the pane itself AND its whole tab:
                    // closing a tab emits only tab_closed, so the panes inside
                    // it are claimed through their tab's mapped local id
                    let tab_closed = pane_tab
                        .get(rid.as_str())
                        .and_then(|t| state.tabs.get(*t))
                        .is_some_and(|e| user_closed.contains(&e.local_id));
                    if close_remote && (user_closed.contains(&entry.local_id) || tab_closed) {
                        pane_close_remote.push(rid.clone());
                    } else {
                        log.log(&format!(
                            "pane mirror for {rid} was closed locally — tombstoning"
                        ));
                    }
                }
                _ => drop_panes.push(rid.clone()),
            }
        }
    }
    for rid in &pane_close_remote {
        log.log(&format!(
            "pane mirror for {rid} closed locally — closing remote pane"
        ));
        if let Err(e) = deps
            .remote
            .request("pane.close", json!({ "pane_id": rid }))
            .await
        {
            log.log(&format!("remote pane close failed for {rid}: {e}"));
        }
    }
    for rid in drop_panes {
        state.panes.remove(&rid);
    }

    // 2. remote objects that disappeared → close their mirrors. Explicit
    //    `*.closed` events are the authoritative close path (see apply_remote_closes);
    //    this snapshot-absence sweep is only a backstop for missed events, and it
    //    acts only when the object was ALSO absent last pass — so a remote that
    //    reconnected mid-restore (transiently empty/partial snapshot) can't
    //    mass-close mirrors.
    let prev_ids = std::mem::take(&mut state.prev_remote_ids);
    let absent_twice =
        |rid: &str, present: &HashSet<&str>| !present.contains(rid) && !prev_ids.contains(rid);
    let gone_ws: Vec<String> = state
        .workspaces
        .keys()
        .filter(|rid| absent_twice(rid, &remote_ws_ids))
        .cloned()
        .collect();
    for rid in gone_ws {
        let entry = state.workspaces.remove(&rid).unwrap();
        if !entry.is_tombstoned() && local_ws_ids.contains(&entry.local_id) {
            log.log(&format!(
                "remote workspace {rid} gone — closing mirror {}",
                entry.local_id
            ));
            mark_self_close(deps, &entry.local_id);
            if let Err(e) = deps
                .local
                .request("workspace.close", json!({ "workspace_id": entry.local_id }))
                .await
            {
                log.log(&format!("close failed: {e}"));
            }
        }
    }
    let gone_tabs: Vec<String> = state
        .tabs
        .keys()
        .filter(|rid| absent_twice(rid, &remote_tab_ids))
        .cloned()
        .collect();
    for rid in gone_tabs {
        let entry = state.tabs.remove(&rid).unwrap();
        if local_tab_ids.contains(entry.local_id.as_str()) {
            let _ = deps
                .local
                .request("tab.close", json!({ "tab_id": entry.local_id }))
                .await;
        }
    }
    let gone_panes: Vec<String> = state
        .panes
        .keys()
        .filter(|rid| absent_twice(rid, &remote_pane_ids))
        .cloned()
        .collect();
    // streamer keys: needed to find a copy whose stored id went stale, and to
    // sweep orphans. Costs one process_info per mirror pane, so only when some
    // mirror pane is unmapped or a remote pane just went away.
    let mapped_now: HashSet<&str> = state.panes.values().map(|e| e.local_id.as_str()).collect();
    let need_keys = !gone_panes.is_empty()
        || local_snap
            .panes
            .iter()
            .any(|p| pane_is_mirror(p) && !mapped_now.contains(p.pane_id.as_str()));
    let keys = if need_keys {
        collect_streamer_keys(&deps.local, &local_snap, &deps.host.target).await
    } else {
        StreamerKeys::new()
    };
    for rid in gone_panes {
        // close the copy (by streamer key, else the stored id), then drop the
        // entry only once the close is settled — a transient failure keeps it
        let plan = plan_pane_reap(&rid, state, &local_snap, &keys);
        let id = state.panes.get(&rid).map(|e| e.local_id.clone()).unwrap_or_default();
        if execute_pane_reap(&deps.local, log, &rid, &id, plan, |l| mark_self_close(deps, l)).await {
            state.panes.remove(&rid);
        }
    }
    // orphans: mirror panes streaming a remote pane that is neither mapped nor
    // on the remote (the copies earlier passes left behind), after 2 passes
    let (orphans, seen) =
        plan_orphan_sweep(state, &local_snap, &remote_pane_ids, &keys, &state.orphan_seen);
    state.orphan_seen = seen;
    for (rid, id) in orphans {
        log.log(&format!(
            "orphan mirror pane {id} streams {rid}, unmapped and absent remotely — closing"
        ));
        mark_self_close(deps, &id);
        match deps.local.request("pane.close", json!({ "pane_id": id })).await {
            Ok(_) => {}
            Err(e) if e.to_string().contains("not found") => {}
            Err(e) => {
                log.log(&format!("orphan close of {id} failed: {e} — will retry"));
                state.orphan_seen.insert(id);
            }
        }
    }
    // record this pass's remote ids for the next comparison
    state.prev_remote_ids = remote_ws_ids
        .iter()
        .chain(remote_tab_ids.iter())
        .chain(remote_pane_ids.iter())
        .map(|s| s.to_string())
        .collect();

    // skip remote workspaces that are entirely another herdr-mirror's streamer
    // panes (a machine mirroring us back), so mutual mirroring can't nest.
    let mut panes_by_ws: HashMap<&str, Vec<&PaneInfo>> = HashMap::new();
    for p in &remote_snap.panes {
        panes_by_ws
            .entry(p.workspace_id.as_str())
            .or_default()
            .push(p);
    }
    let mut mirror_ws_ids: HashSet<String> = HashSet::new();
    for rws in &remote_snap.workspaces {
        let Some(panes) = panes_by_ws
            .get(rws.workspace_id.as_str())
            .filter(|p| !p.is_empty())
        else {
            continue;
        };
        if panes.iter().all(|p| pane_is_mirror(p)) {
            mirror_ws_ids.insert(rws.workspace_id.clone());
        }
    }

    // 3. remote workspaces → ensure mirrors exist with the right label
    for rws in &remote_snap.workspaces {
        if mirror_ws_ids.contains(&rws.workspace_id) {
            continue;
        }
        let label = format!("{}: {}", host.prefix, rws.label);
        if state
            .workspaces
            .get(&rws.workspace_id)
            .is_some_and(|e| e.is_tombstoned())
        {
            continue;
        }
        let existing = state
            .workspaces
            .get(&rws.workspace_id)
            .filter(|e| local_ws_ids.contains(&e.local_id))
            .cloned();
        if let Some(entry) = existing {
            let local_ws = local_snap
                .workspaces
                .iter()
                .find(|w| w.workspace_id == entry.local_id);
            if let Some(lws) = local_ws {
                match resolve_label(
                    Some(&host.prefix),
                    &rws.label,
                    &lws.label,
                    entry.last_remote_label.as_deref(),
                ) {
                    LabelAction::PushRemote(new_remote) => {
                        // the user renamed the mirror → the rename is intent for
                        // the REMOTE workspace; push it there and restamp local
                        // with the canonical "<prefix>: <name>" form
                        log.log(&format!(
                            "local rename of {} → pushing \"{new_remote}\" to remote {}",
                            lws.label, rws.workspace_id
                        ));
                        deps.remote
                            .request(
                                "workspace.rename",
                                json!({ "workspace_id": rws.workspace_id, "label": new_remote }),
                            )
                            .await?;
                        let stamped = format!("{}: {}", host.prefix, new_remote);
                        if lws.label != stamped {
                            deps.local
                                .request(
                                    "workspace.rename",
                                    json!({ "workspace_id": entry.local_id, "label": stamped }),
                                )
                                .await?;
                        }
                        if let Some(e) = state.workspaces.get_mut(&rws.workspace_id) {
                            e.last_remote_label = Some(new_remote);
                        }
                    }
                    LabelAction::RestampLocal => {
                        deps.local
                            .request(
                                "workspace.rename",
                                json!({ "workspace_id": entry.local_id, "label": label }),
                            )
                            .await?;
                        if let Some(e) = state.workspaces.get_mut(&rws.workspace_id) {
                            e.last_remote_label = Some(rws.label.clone());
                        }
                    }
                    LabelAction::InSync => {
                        if entry.last_remote_label.as_deref() != Some(rws.label.as_str()) {
                            if let Some(e) = state.workspaces.get_mut(&rws.workspace_id) {
                                e.last_remote_label = Some(rws.label.clone());
                            }
                        }
                    }
                }
            }
        } else {
            // adopt a label-matching unmapped local workspace (orphan from a crash)
            let mapped: HashSet<&str> = state
                .workspaces
                .values()
                .map(|e| e.local_id.as_str())
                .collect();
            let orphan = local_snap
                .workspaces
                .iter()
                .find(|w| w.label == label && !mapped.contains(w.workspace_id.as_str()));
            let entry = if let Some(orphan) = orphan {
                log.log(&format!(
                    "adopting existing workspace {label} ({})",
                    orphan.workspace_id
                ));
                WsEntry {
                    local_id: orphan.workspace_id.clone(),
                    tombstone: None,
                    root_tab_local_id: if orphan.tab_count == Some(1)
                        && orphan.pane_count == Some(1)
                    {
                        orphan.active_tab_id.clone()
                    } else {
                        None
                    },
                    last_remote_label: Some(rws.label.clone()),
                }
            } else {
                log.log(&format!("creating mirror workspace {label}"));
                #[derive(Deserialize)]
                struct Created {
                    workspace: CreatedWs,
                    tab: CreatedTab,
                }
                #[derive(Deserialize)]
                struct CreatedWs {
                    workspace_id: String,
                }
                #[derive(Deserialize)]
                struct CreatedTab {
                    tab_id: String,
                }
                // same non-git marker cwd the mirror panes use, so the
                // workspace's default pane never flashes a (misleading) sidebar
                // git branch before layout.apply swaps in the real mirror panes
                let cwd = mirror_pane_cwd(&deps.state_dir).display().to_string();
                let created: Created = deps
                    .local
                    .request_t(
                        "workspace.create",
                        json!({ "label": label, "cwd": cwd, "focus": false }),
                    )
                    .await?;
                WsEntry {
                    local_id: created.workspace.workspace_id,
                    tombstone: None,
                    root_tab_local_id: Some(created.tab.tab_id),
                    last_remote_label: Some(rws.label.clone()),
                }
            };
            local_ws_ids.insert(entry.local_id.clone());
            let fresh: Vec<String> = std::iter::once(entry.local_id.clone())
                .chain(entry.root_tab_local_id.clone())
                .collect();
            state.workspaces.insert(rws.workspace_id.clone(), entry);
            note_mapped(deps, state, &fresh);
        }
    }

    // 3b. forward the remote's workspace tokens onto the mirror rows, so a mirror
    //     carries the same values a native workspace does under whatever layout is
    //     configured locally. Ignored by a pre-0.7.4 local server.
    let source = mirror_source(&host.name);
    for rws in &remote_snap.workspaces {
        if rws.tokens.is_empty() {
            continue; // nothing to forward (also the pre-0.7.4 remote case)
        }
        let Some(entry) = state.workspaces.get(&rws.workspace_id) else {
            continue;
        };
        if entry.is_tombstoned() || !local_ws_ids.contains(&entry.local_id) {
            continue;
        }
        let _ = deps
            .local
            .request(
                "workspace.report_metadata",
                json!({ "workspace_id": entry.local_id, "source": source, "tokens": rws.tokens }),
            )
            .await;
    }

    // 4. remote tabs → replicate layout with wrapper commands
    for rtab in &remote_snap.tabs {
        let Some(ws_entry) = state.workspaces.get(&rtab.workspace_id).cloned() else {
            continue;
        };
        if ws_entry.is_tombstoned() {
            continue;
        }
        let tab_entry = state.tabs.get(&rtab.tab_id).cloned();
        let tab_exists = tab_entry
            .as_ref()
            .is_some_and(|t| local_tab_ids.contains(t.local_id.as_str()));
        let remote_panes_in_tab: Vec<&PaneInfo> = remote_snap
            .panes
            .iter()
            .filter(|p| p.tab_id == rtab.tab_id)
            .collect();
        // A tab whose mirror the user closed leaves only tombstoned pane
        // entries behind (a TabEntry has no tombstone of its own — its stale
        // local id just stops resolving). Rebuilding it would recreate panes
        // the tombstones then forbid wiring up; skip before the layout.export
        // round-trip. `restore` deletes the tombstones, which lifts this.
        if !tab_exists
            && !remote_panes_in_tab.is_empty()
            && remote_panes_in_tab.iter().all(|p| {
                state
                    .panes
                    .get(&p.pane_id)
                    .is_some_and(|e| e.is_tombstoned())
            })
        {
            continue;
        }

        if !tab_exists
            || remote_panes_in_tab
                .iter()
                .any(|p| !state.panes.contains_key(&p.pane_id))
        {
            #[derive(Deserialize)]
            struct Exported {
                layout: ExportedLayout,
            }
            #[derive(Deserialize)]
            struct ExportedLayout {
                root: LayoutNode,
            }
            let exported: Exported = deps
                .remote
                .request_t("layout.export", json!({ "tab_id": rtab.tab_id }))
                .await?;

            if !tab_exists {
                // apply only the non-tombstoned part of the tree: layout.apply
                // creates a real local pane per leaf, so a tombstoned leaf
                // would materialize as a titled dead shell in the marker cwd
                // that the mapping loop below then can't wire a streamer into
                let Some(live_root) = prune_closed(&exported.layout.root, &state.panes) else {
                    continue;
                };
                let mut remote_order = Vec::new();
                walk_pane_ids(&live_root, &mut remote_order);
                // non-git cwd so herdr shows no (misleading) sidebar git status
                // for the mirror; the pane exec's the streamer regardless
                let cwd = mirror_pane_cwd(&deps.state_dir).display().to_string();
                let root = map_node(&live_root, &cwd);
                let target_tab = ws_entry.root_tab_local_id.clone();
                // tab_id and workspace_id are mutually exclusive on layout.apply
                let mut params = json!({ "tab_label": rtab.label, "root": root, "focus": false });
                match &target_tab {
                    Some(t) => params["tab_id"] = json!(t),
                    None => params["workspace_id"] = json!(ws_entry.local_id),
                }
                #[derive(Deserialize)]
                struct Applied {
                    layout: AppliedLayout,
                }
                #[derive(Deserialize)]
                struct AppliedLayout {
                    tab_id: String,
                    root: LayoutNode,
                }
                let applied: Applied = deps.local.request_t("layout.apply", params).await?;
                // consume the root tab only AFTER a successful apply, so a
                // transient failure retries against it instead of stacking a tab
                if let Some(ws) = state.workspaces.get_mut(&rtab.workspace_id) {
                    ws.root_tab_local_id = None;
                }
                // applied with `tab_label: rtab.label`, so the two agree from birth
                let mut fresh = vec![applied.layout.tab_id.clone()];
                state.tabs.insert(
                    rtab.tab_id.clone(),
                    crate::state::TabEntry {
                        local_id: applied.layout.tab_id,
                        last_remote_label: Some(rtab.label.clone()),
                    },
                );
                let mut local_order = Vec::new();
                walk_pane_ids(&applied.layout.root, &mut local_order);
                // map every pane first and persist, THEN exec streamers: the
                // panes already exist (layout.apply made them), so the map
                // write must not wait behind the send_text round-trips
                let mut to_spawn: Vec<(String, String)> = Vec::new();
                for (i, rid) in remote_order.iter().enumerate() {
                    if rid.is_empty() || local_order.get(i).is_none_or(|l| l.is_empty()) {
                        continue;
                    }
                    let local_id = local_order[i].clone();
                    let seq = state.panes.get(rid).map(|e| e.seq).unwrap_or(0);
                    state.panes.insert(
                        rid.clone(),
                        PaneEntry {
                            local_id: local_id.clone(),
                            tombstone: None,
                            seq,
                            reported: None,
                        },
                    );
                    fresh.push(local_id.clone());
                    to_spawn.push((local_id, rid.clone()));
                }
                note_mapped(deps, state, &fresh);
                for (local_id, rid) in &to_spawn {
                    // plain pane created above; exec the streamer into it
                    spawn_streamer_pane(
                        &deps.local,
                        &deps.state_dir,
                        local_id,
                        &cmd_for(rid),
                        &deps.log,
                    )
                    .await;
                }
            } else {
                // tab exists — add mirrors for individual new remote panes as
                // PLAIN split panes (not agent.start), then exec the streamer in.
                // agent.start would set launch_argv and surface every plain
                // terminal as a phantom "mirror" agent row.
                // non-git cwd so herdr shows no (misleading) sidebar git status
                // for the mirror; the pane exec's the streamer regardless
                let cwd = mirror_pane_cwd(&deps.state_dir).display().to_string();
                // Place new panes where the REMOTE tree says they live, in
                // dependency order, so a burst of several (a converge that fell
                // behind, or a whole nested tab) reproduces the remote's shape
                // instead of flattening every new pane onto one target. Each
                // split carries the remote split's ratio; `swap` covers the
                // case where the remote has the new pane as the FIRST child,
                // which pane.split can't do directly.
                let mirrored: std::collections::BTreeSet<String> = state
                    .panes
                    .iter()
                    .filter(|(_, e)| !e.is_tombstoned())
                    .map(|(rid, _)| rid.clone())
                    .collect();
                let (placements, unplaceable) =
                    crate::layout_sync::plan_placements(&exported.layout.root, &mirrored);
                let in_this_tab =
                    |rid: &String| remote_panes_in_tab.iter().any(|p| &p.pane_id == rid);
                for place in placements.iter().filter(|p| in_this_tab(&p.pane)) {
                    if state.panes.contains_key(&place.pane) {
                        continue;
                    }
                    let Some(target) = state.panes.get(&place.target).map(|e| e.local_id.clone())
                    else {
                        continue;
                    };
                    let adopted = orphan_gc::find_copy(&deps.local, &deps.state_dir, &deps.host.target, &place.pane, state).await;
                    let was_adopted = adopted.is_some();
                    let local_id = match adopted {
                        Some(id) => id,
                        None => {
                            split_mirror_pane(
                                &deps.local,
                                &target,
                                &place.direction,
                                Some(place.ratio),
                                &cwd,
                                &place.pane,
                            )
                            .await?
                        }
                    };
                    // the remote has this pane on the split's first side, and
                    // pane.split always lands the new one second. Swapping puts
                    // it where the remote has it; the split's ratio rides along
                    // untouched, so the geometry matches exactly.
                    if place.swap && !was_adopted {
                        let _ = deps
                            .local
                            .request(
                                "pane.swap",
                                json!({ "source_pane_id": local_id, "target_pane_id": target }),
                            )
                            .await;
                    }
                    // map + persist BEFORE the streamer exec: the pane exists
                    // as of pane.split above, and the intercept hook judges
                    // unmapped placeholder panes 250ms after pane.created
                    state.panes.insert(
                        place.pane.clone(),
                        PaneEntry {
                            local_id: local_id.clone(),
                            tombstone: None,
                            seq: 0,
                            reported: None,
                        },
                    );
                    note_mapped(deps, state, std::slice::from_ref(&local_id));
                    spawn_streamer_pane(
                        &deps.local,
                        &deps.state_dir,
                        &local_id,
                        &cmd_for(&place.pane),
                        &deps.log,
                    )
                    .await;
                }
                // A pane whose remote sibling is a multi-pane subtree can't be
                // reproduced: pane.split splits a leaf, and nothing wraps a
                // subtree in a new split. Place it by the old heuristic so the
                // mirror is never missing a pane, and say so — the tab's ratio
                // sync will report a structural mismatch from here on.
                for rp in remote_panes_in_tab
                    .iter()
                    .filter(|p| unplaceable.contains(&p.pane_id))
                {
                    if state.panes.contains_key(&rp.pane_id) {
                        continue;
                    }
                    let fallback = locate_in_layout(&exported.layout.root, &rp.pane_id)
                        .and_then(|(dir, sibs)| {
                            sibs.iter()
                                .find_map(|rid| state.panes.get(rid).map(|e| e.local_id.clone()))
                                .map(|t| (t, dir))
                        })
                        .or_else(|| {
                            remote_panes_in_tab
                                .iter()
                                .find_map(|p| {
                                    state.panes.get(&p.pane_id).map(|e| e.local_id.clone())
                                })
                                .map(|t| (t, "right".to_string()))
                        });
                    let Some((target, direction)) = fallback else {
                        continue;
                    };
                    log.log(&format!(
                        "{}: remote pane {} sits beside a subtree — mirroring it beside {target} instead; split sizes for this tab won't track",
                        rtab.tab_id, rp.pane_id
                    ));
                    let local_id = match orphan_gc::find_copy(&deps.local, &deps.state_dir, &deps.host.target, &rp.pane_id, state).await {
                        Some(id) => id,
                        None => {
                            split_mirror_pane(
                                &deps.local,
                                &target,
                                &direction,
                                None,
                                &cwd,
                                &rp.pane_id,
                            )
                            .await?
                        }
                    };
                    state.panes.insert(
                        rp.pane_id.clone(),
                        PaneEntry {
                            local_id: local_id.clone(),
                            tombstone: None,
                            seq: 0,
                            reported: None,
                        },
                    );
                    note_mapped(deps, state, std::slice::from_ref(&local_id));
                    spawn_streamer_pane(
                        &deps.local,
                        &deps.state_dir,
                        &local_id,
                        &cmd_for(&rp.pane_id),
                        &deps.log,
                    )
                    .await;
                }
            }
        }

        if tab_exists {
            let entry = tab_entry.as_ref().unwrap();
            let tab_local = &entry.local_id;
            let local_tab = local_snap.tabs.iter().find(|t| &t.tab_id == tab_local);
            // Same two-way resolution the workspace labels get above: a local
            // rename is intent for the REMOTE tab, and only a remote that moved
            // since we last stamped may overwrite the local label.
            if let Some(ltab) = local_tab {
                match resolve_label(
                    None,
                    &rtab.label,
                    &ltab.label,
                    entry.last_remote_label.as_deref(),
                ) {
                    LabelAction::PushRemote(new_remote) => {
                        log.log(&format!(
                            "local rename of tab {tab_local} → pushing \"{new_remote}\" to remote {}",
                            rtab.tab_id
                        ));
                        // record the new label only once the remote has it, so a
                        // failed push is retried by the next converge instead of
                        // being mistaken for a remote rename and stomped
                        if deps
                            .remote
                            .request(
                                "tab.rename",
                                json!({ "tab_id": rtab.tab_id, "label": new_remote }),
                            )
                            .await
                            .is_ok()
                        {
                            if let Some(e) = state.tabs.get_mut(&rtab.tab_id) {
                                e.last_remote_label = Some(new_remote);
                            }
                        }
                    }
                    LabelAction::RestampLocal => {
                        // same discipline as the push above, for the same reason
                        // in reverse: recording a label the local tab never took
                        // makes the next converge read the stale local one as a
                        // user rename and push it over the remote's
                        if deps
                            .local
                            .request(
                                "tab.rename",
                                json!({ "tab_id": tab_local, "label": rtab.label }),
                            )
                            .await
                            .is_ok()
                        {
                            if let Some(e) = state.tabs.get_mut(&rtab.tab_id) {
                                e.last_remote_label = Some(rtab.label.clone());
                            }
                        }
                    }
                    LabelAction::InSync => {
                        if entry.last_remote_label.as_deref() != Some(rtab.label.as_str()) {
                            if let Some(e) = state.tabs.get_mut(&rtab.tab_id) {
                                e.last_remote_label = Some(rtab.label.clone());
                            }
                        }
                    }
                }
            }

            // Placement above only sets a ratio for a split it JUST created. A
            // resize of an existing split has no topology change to hang off
            // of, so it needs its own check: `layout.updated` is subscribed on
            // both sides (see daemon.rs), and each converge diffs the two
            // exports and moves whichever side didn't change.
            //
            // A tab with fewer than two panes has no split to reconcile, which
            // is most tabs, so it costs nothing there.
            if remote_panes_in_tab.len() > 1 {
                reconcile_tab_geometry(deps, state, &rtab.tab_id, tab_local, &remote_panes_in_tab)
                    .await;
            }
        }
    }

    // remembered ratio agreements for tabs that are gone would otherwise
    // accumulate in the state file forever
    let live_tabs: HashSet<String> = state.tabs.keys().cloned().collect();
    state
        .ratios
        .retain(|k, _| k.split('|').next().is_some_and(|t| live_tabs.contains(t)));

    // 5. push authoritative agent status onto mirror panes
    push_statuses(deps, &remote_snap, state).await;
    if deps.orphan_gc {
        orphan_gc::collect(deps, state).await;
    } else {
        state.orphan_candidates.clear();
    }
    Ok(())
}

/// Push one pane's authoritative status (or retract it when the remote agent
/// is gone). Mutates only its own entry (seq/reported). Reused by both the
/// full converge and the daemon's status fast-path.
pub async fn push_pane_status(
    local: &ApiClient,
    host_name: &str,
    remote_id: &str,
    entry: &mut PaneEntry,
    agent: Option<&AgentInfo>,
    log: &Logger,
) {
    if entry.is_tombstoned() {
        return;
    }
    let source = mirror_source(host_name);
    match agent {
        Some(agent) => {
            entry.seq += 1;
            let display = agent.display_agent.clone().or_else(|| agent.agent.clone());
            // Identity is the remote's CANONICAL id ("claude"), not the pretty
            // name: herdr canonicalizes a reported label, so this resolves the
            // real agent and the mirror row inherits its rows_by_agent layout and
            // icon instead of rendering as a nameless custom agent. The pretty
            // name still goes out below as display_agent, which is what the
            // sidebar actually shows.
            let label = agent
                .agent
                .clone()
                .or_else(|| display.clone())
                .unwrap_or_else(|| "agent".into());
            // pass through only a custom status the remote actually reports;
            // no synthetic "@host" marker (clear any stale one)
            let custom: Option<String> = agent.custom_status.as_deref().map(clamp_status);
            let status = agent.agent_status.as_deref().unwrap_or("unknown");
            let mut report = json!({
                "pane_id": entry.local_id,
                "source": source,
                "agent": label,
                "state": map_status(status),
                "seq": entry.seq,
            });
            if let Some(c) = &custom {
                report["custom_status"] = json!(c);
            }
            if let Err(e) = local.request("pane.report_agent", report).await {
                log.log(&format!("report_agent {}: {e}", entry.local_id));
            }
            // forward the remote's own tokens so a mirrored agent row carries the
            // same values a native one does, under whatever layout is configured
            // locally. Ignored by a pre-0.7.4 local server (no deny_unknown_fields).
            let mut meta = json!({
                "pane_id": entry.local_id,
                "source": source,
                "display_agent": display,
                "title": agent.effective_title(),
                "state_labels": agent.state_labels.clone().unwrap_or_default(),
                "tokens": agent.tokens.clone(),
                "seq": entry.seq,
            });
            if custom.is_none() {
                meta["clear_custom_status"] = json!(true);
            }
            let _ = local.request("pane.report_metadata", meta).await;
            entry.reported = Some(label);
        }
        None => {
            let Some(reported) = entry.reported.clone() else {
                return;
            };
            // remote agent exited — retract our claim so the mirror pane doesn't
            // show a phantom agent row forever
            entry.seq += 1;
            log.log(&format!(
                "remote agent gone on {remote_id} — releasing {reported} from {}",
                entry.local_id
            ));
            if let Err(e) = local
                .request(
                    "pane.release_agent",
                    json!({ "pane_id": entry.local_id, "source": source, "agent": reported, "seq": entry.seq }),
                )
                .await
            {
                log.log(&format!("release_agent {}: {e}", entry.local_id));
            }
            entry.seq += 1;
            let _ = local
                .request(
                    "pane.report_metadata",
                    json!({
                        "pane_id": entry.local_id,
                        "source": source,
                        "clear_display_agent": true,
                        "clear_custom_status": true,
                        "clear_state_labels": true,
                        "clear_title": true,
                        "seq": entry.seq,
                    }),
                )
                .await;
            entry.reported = None;
        }
    }
}

/// Authoritative close path: apply explicit remote `*.closed` events by closing
/// the matching local mirror and pruning state. Ids are namespaced (ws `w1`, tab
/// `w1:t1`, pane `w1:p1`), so each is looked up wherever it lives. Closing a
/// workspace mirror cascades to its tabs/panes locally; stale child state entries
/// are pruned by the next converge.
pub async fn apply_remote_closes(
    local: &ApiClient,
    state_dir: &std::path::Path,
    host_name: &str,
    closed: &[String],
    log: &Logger,
) {
    if closed.is_empty() {
        return;
    }
    let mut state = load_state(state_dir, host_name);
    let mut changed = false;
    for rid in closed {
        if let Some(entry) = state.workspaces.remove(rid) {
            changed = true;
            if !entry.is_tombstoned() {
                log.log(&format!(
                    "remote workspace {rid} closed — closing mirror {}",
                    entry.local_id
                ));
                let _ = local
                    .request("workspace.close", json!({ "workspace_id": entry.local_id }))
                    .await;
            }
        } else if let Some(entry) = state.tabs.remove(rid) {
            changed = true;
            let _ = local
                .request("tab.close", json!({ "tab_id": entry.local_id }))
                .await;
        } else if state.panes.contains_key(rid) {
            let Ok(local_snap) = fetch_snapshot(local).await else {
                log.log(&format!(
                    "remote pane {rid} closed — local snapshot unavailable, keeping mapping"
                ));
                continue;
            };
            let plan = plan_pane_reap(rid, &state, &local_snap, &StreamerKeys::new());
            let id = state.panes.get(rid).map(|e| e.local_id.clone()).unwrap_or_default();
            // no self-close mark here: this path never had one (closes.rs echo is
            // handled by the daemon event loop for explicit remote closes)
            if execute_pane_reap(local, log, rid, &id, plan, |_| {}).await {
                state.panes.remove(rid);
                changed = true;
            }
        }
    }
    if changed {
        if let Err(e) = save_state(state_dir, host_name, &state) {
            log.log(&format!("[{host_name}] state save failed: {e}"));
        }
    }
}

pub async fn push_statuses(deps: &ConvergeDeps, remote_snap: &Snapshot, state: &mut HostState) {
    let agent_by_pane: HashMap<&str, &AgentInfo> = remote_snap
        .agents
        .iter()
        .map(|a| (a.pane_id.as_str(), a))
        .collect();
    for (remote_id, entry) in state.panes.iter_mut() {
        let agent = agent_by_pane.get(remote_id.as_str()).copied();
        push_pane_status(
            &deps.local,
            &deps.host.name,
            remote_id,
            entry,
            agent,
            &deps.log,
        )
        .await;
    }
}

/// Mark mirrored agents unknown (ssh drop) — statuses recover on reconnect.
/// Only panes we actually reported an agent onto; inventing agent rows for
/// plain mirrored terminals pollutes the agents panel.
pub async fn mark_unknown(
    local: &ApiClient,
    state_dir: &std::path::Path,
    host_name: &str,
    reason: &str,
) {
    let mut state = load_state(state_dir, host_name);
    let source = mirror_source(host_name);
    let custom = clamp_status(reason);
    for entry in state.panes.values_mut() {
        let Some(reported) = entry.reported.clone() else {
            continue;
        };
        if entry.is_tombstoned() {
            continue;
        }
        entry.seq += 1;
        let _ = local
            .request(
                "pane.report_agent",
                json!({
                    "pane_id": entry.local_id,
                    "source": source,
                    "agent": reported,
                    "state": "unknown",
                    "custom_status": custom,
                    "seq": entry.seq,
                }),
            )
            .await;
    }
    let _ = save_state(state_dir, host_name, &state);
}

/// Graceful teardown: close every mirror workspace this host created.
pub async fn teardown(
    local: &ApiClient,
    state_dir: &std::path::Path,
    host_name: &str,
    log: &Logger,
    closes: Option<&crate::closes::Closes>,
) -> Result<()> {
    let state = load_state(state_dir, host_name);
    // Wipe the id map BEFORE closing the local windows. teardown (and the
    // restart / zombie-heal that call it) means "stop mirroring here" — never
    // "close the remote sessions". But close_remote_on_local_close fires when a
    // converge sees a still-mapped mirror vanish locally, and it can't tell our
    // bulk close from the user pressing prefix-x. Clearing the map first leaves
    // nothing to attribute these closes to, so they cannot propagate to the
    // remote. Manual close is unaffected: there the entry is still mapped when
    // the user closes it, so the intent still reaches the remote.
    save_state(state_dir, host_name, &HostState::default())?;
    // teardown means "stop mirroring here entirely", which supersedes hide —
    // leaving the marker would make a later `start` bring back nothing with no
    // explanation of why
    let _ = crate::state::set_hidden(state_dir, host_name, false);
    for entry in state.workspaces.values() {
        log.log(&format!("closing mirror workspace {}", entry.local_id));
        // ours, not the user's: the heal re-adopts these ids, so without the mark
        // the echoing close event would later read as "user closed the mirror"
        if let Some(c) = closes {
            if let Ok(mut t) = c.lock() {
                t.mark_self_close(&entry.local_id);
            }
        }
        let _ = local
            .request("workspace.close", json!({ "workspace_id": entry.local_id }))
            .await;
    }
    Ok(())
}

async fn move_ws(local: &ApiClient, ws: &str, insert_index: usize) -> bool {
    local
        .request(
            "workspace.move",
            json!({ "workspace_id": ws, "insert_index": insert_index }),
        )
        .await
        .is_ok()
}

/// rank a workspace by its label: local (no `<prefix>: `) sorts first (0), then
/// each host's mirrors by config order (i+1). First matching prefix wins.
fn ws_rank(label: &str, prefixes: &[String]) -> usize {
    for (i, p) in prefixes.iter().enumerate() {
        if label.starts_with(&format!("{p}: ")) {
            return i + 1;
        }
    }
    0
}

/// Pure planner: given the current `(workspace_id, rank)` order, return the
/// `(workspace_id, insert_index)` workspace.move calls that group the sidebar
/// (locals first, then mirror ranks ascending, preserving order within each
/// group), moving ONLY mirror rows (rank > 0). Empty when already grouped.
///
/// `insert_index` is herdr's pre-removal gap index: pulling a row up lands it at
/// `i`; pushing one to the end uses `insert_index == len`.
fn plan_regroup(current: &[(String, usize)]) -> Vec<(String, usize)> {
    let mut target = current.to_vec();
    target.sort_by_key(|(_, r)| *r); // stable: preserves order within each group
    if current == target.as_slice() {
        return Vec::new();
    }
    let mut moves = Vec::new();
    let mut working = current.to_vec();
    let n = working.len();
    let mut i = 0usize;
    let mut guard = 0usize;
    while i < target.len() {
        guard += 1;
        if guard > n * n + 8 {
            break;
        }
        if working[i].0 == target[i].0 {
            i += 1;
            continue;
        }
        if target[i].1 > 0 {
            // a mirror belongs at i and is currently later — pull it up to i
            let want = target[i].0.clone();
            let src = working.iter().position(|(id, _)| *id == want).unwrap();
            moves.push((want.clone(), i));
            let item = working.remove(src);
            working.insert(i, item);
            i += 1;
        } else if i + 1 < working.len() {
            // a local belongs at i but a mirror sits there — push that mirror to the end
            let m = working[i].0.clone();
            moves.push((m.clone(), working.len()));
            let item = working.remove(i);
            working.push(item);
        } else {
            i += 1;
        }
    }
    moves
}

/// Keep the local sidebar grouped: local (non-mirror) workspaces first, then each
/// host's mirror workspaces contiguous in config order. Classifies by the
/// `<prefix>: ` label the mirror sets, and only ever moves mirror rows — local
/// workspaces are never reordered (they group as a side effect of mirror rows
/// being pushed below them). Idempotent: issues no moves when already grouped.
pub async fn regroup_sidebar(local: &ApiClient, prefixes: &[String], log: &Logger) {
    let Ok(snap) = fetch_snapshot(local).await else {
        return;
    };
    let current: Vec<(String, usize)> = snap
        .workspaces
        .iter()
        .map(|w| (w.workspace_id.clone(), ws_rank(&w.label, prefixes)))
        .collect();
    for (ws, insert_index) in plan_regroup(&current) {
        if !move_ws(local, &ws, insert_index).await {
            log.log(&format!("regroup: move {ws} failed"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::hidden_close_plan;

    // --- M1: orphaned mirror panes ---

    use super::{execute_pane_reap, plan_pane_reap, PaneReap, PaneInfo, Snapshot, AgentInfo};
    use crate::api::ApiClient;
    use crate::state::PaneEntry;
    use crate::util::Logger;
    use std::sync::{Arc, Mutex};

    fn no_keys() -> super::StreamerKeys {
        super::StreamerKeys::new()
    }

    fn pane_entry(local: &str, tomb: bool) -> PaneEntry {
        PaneEntry { local_id: local.into(), tombstone: tomb.then_some(true), seq: 0, reported: None }
    }

    fn lpane(id: &str, cwd: Option<&str>) -> PaneInfo {
        PaneInfo {
            pane_id: id.into(),
            tab_id: "t".into(),
            workspace_id: "w".into(),
            label: None,
            cwd: cwd.map(String::from),
            foreground_cwd: None,
        }
    }

    fn snap(panes: Vec<PaneInfo>, agent_on: &[&str]) -> Snapshot {
        Snapshot {
            panes,
            agents: agent_on
                .iter()
                .map(|id| AgentInfo { pane_id: id.to_string(), agent: Some("claude".into()), ..Default::default() })
                .collect(),
            ..Default::default()
        }
    }

    fn state_with(rid: &str, e: PaneEntry) -> HostState {
        let mut s = HostState::default();
        s.panes.insert(rid.into(), e);
        s
    }

    #[test]
    fn gone_remote_pane_closes_the_copy_stored_in_the_map() {
        let st = state_with("wN:p1", pane_entry("w6E:p9", false));
        let sn = snap(vec![lpane("w6E:p9", Some("/x/.mirror-pane"))], &["w6E:p9"]);
        assert_eq!(plan_pane_reap("wN:p1", &st, &sn, &no_keys()), PaneReap::Close("w6E:p9".into()));
    }

    #[test]
    fn a_recalculated_id_never_matters_only_the_stored_one() {
        // the converge-order id (w6E:p1) differs from the stored one: the plan
        // still names the stored id, and leaves the other pane alone
        let st = state_with("wN:p1", pane_entry("w6E:p9", false));
        let sn = snap(vec![lpane("w6E:p1", None), lpane("w6E:p9", Some("/x/.mirror-pane"))], &[]);
        assert_eq!(plan_pane_reap("wN:p1", &st, &sn, &no_keys()), PaneReap::Close("w6E:p9".into()));
    }

    #[test]
    fn pane_outside_the_map_is_never_closed() {
        let st = state_with("wN:p1", pane_entry("w6E:p9", false));
        let sn = snap(vec![lpane("w6E:p5", Some("/x/.mirror-pane"))], &[]);
        assert_eq!(plan_pane_reap("wN:p1", &st, &sn, &no_keys()), PaneReap::Absent);
        assert_eq!(plan_pane_reap("wN:unknown", &st, &sn, &no_keys()), PaneReap::Absent);
    }

    #[test]
    fn foreign_live_agent_on_reused_id_is_refused() {
        let st = state_with("wN:p1", pane_entry("w6E:p9", false));
        let sn = snap(vec![lpane("w6E:p9", Some("/home/u/proj"))], &["w6E:p9"]);
        assert!(matches!(plan_pane_reap("wN:p1", &st, &sn, &no_keys()), PaneReap::Refuse(_)));
    }

    #[test]
    fn tombstoned_empty_and_shared_ids_are_not_closed() {
        let sn = snap(vec![lpane("w6E:p9", None)], &[]);
        assert_eq!(plan_pane_reap("r", &state_with("r", pane_entry("w6E:p9", true)), &sn, &no_keys()), PaneReap::Absent);
        assert!(matches!(
            plan_pane_reap("r", &state_with("r", pane_entry("", false)), &sn, &no_keys()),
            PaneReap::Refuse(_)
        ));
        let mut st = state_with("r", pane_entry("w6E:p9", false));
        st.panes.insert("r2".into(), pane_entry("w6E:p9", false));
        assert!(matches!(plan_pane_reap("r", &st, &sn, &no_keys()), PaneReap::Refuse(_)));
    }

    /// Fake herdr socket: records methods, answers pane.close per `reply`.
    async fn fake_herdr(
        dir: &std::path::Path,
        reply: &'static str,
    ) -> (ApiClient, Arc<Mutex<Vec<(String, serde_json::Value)>>>) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let path = dir.join("fake.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((st, _)) = listener.accept().await else { return };
                let seen = seen2.clone();
                tokio::spawn(async move {
                    let (r, mut w) = st.into_split();
                    let mut lines = BufReader::new(r).lines();
                    if let Ok(Some(l)) = lines.next_line().await {
                        let v: serde_json::Value = serde_json::from_str(&l).unwrap();
                        seen.lock().unwrap().push((
                            v["method"].as_str().unwrap().to_string(),
                            v["params"].clone(),
                        ));
                        let out = match reply {
                            "ok" => serde_json::json!({"id": v["id"], "result": {}}),
                            "notfound" => serde_json::json!({"id": v["id"], "error": {"message": "pane w6E:p9 not found"}}),
                            _ => serde_json::json!({"id": v["id"], "error": {"message": "boom"}}),
                        };
                        let _ = w.write_all((out.to_string() + "\n").as_bytes()).await;
                    }
                });
            }
        });
        (ApiClient::connect_unchecked(&path), seen)
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("hm-m1-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn execute_closes_stored_id_and_repeat_is_idempotent() {
        let d = tmpdir("exec");
        let (api, seen) = fake_herdr(&d, "ok").await;
        let log = Logger::new(&d, false);
        let marked = Arc::new(Mutex::new(Vec::new()));
        let m = marked.clone();
        let plan = PaneReap::Close("w6E:p9".into());
        assert!(execute_pane_reap(&api, &log, "wN:p1", "w6E:p9", plan, move |l| m.lock().unwrap().push(l.to_string())).await);
        assert_eq!(marked.lock().unwrap().as_slice(), ["w6E:p9"]);
        {
            let s = seen.lock().unwrap();
            assert_eq!(s.len(), 1);
            assert_eq!(s[0].0, "pane.close");
            assert_eq!(s[0].1["pane_id"], "w6E:p9");
        }
        // repeated warning: entry already dropped / copy gone => no further close
        let st = HostState::default();
        let sn = snap(vec![], &[]);
        let again = plan_pane_reap("wN:p1", &st, &sn, &no_keys());
        assert!(execute_pane_reap(&api, &log, "wN:p1", "", again, |_| {}).await);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn not_found_counts_as_done_and_other_errors_retry() {
        let d = tmpdir("err");
        let (api, _) = fake_herdr(&d, "notfound").await;
        let log = Logger::new(&d, false);
        assert!(execute_pane_reap(&api, &log, "r", "w6E:p9", PaneReap::Close("w6E:p9".into()), |_| {}).await);
        let d2 = tmpdir("err2");
        let (api2, _) = fake_herdr(&d2, "boom").await;
        assert!(!execute_pane_reap(&api2, &log, "r", "w6E:p9", PaneReap::Close("w6E:p9".into()), |_| {}).await);
    }

    #[tokio::test]
    async fn refused_plan_sends_nothing() {
        let d = tmpdir("refuse");
        let (api, seen) = fake_herdr(&d, "ok").await;
        let log = Logger::new(&d, false);
        assert!(execute_pane_reap(&api, &log, "r", "w6E:p9", PaneReap::Refuse("x"), |_| panic!("marked")).await);
        assert!(seen.lock().unwrap().is_empty());
    }

    fn ws_entry(local: &str, tomb: bool) -> WsEntry {
        WsEntry {
            local_id: local.into(),
            tombstone: tomb.then_some(true),
            root_tab_local_id: None,
            last_remote_label: None,
        }
    }

    fn live(ids: &[&str]) -> std::collections::HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// The bug that made the daemon close the mirrors it had just created, on
    /// every pass, for every host. It shipped past a full-green suite because
    /// nothing exercised the gate.
    #[test]
    fn a_host_that_is_not_hidden_is_never_touched() {
        let mut state = HostState::default();
        state.workspaces.insert("R1".into(), ws_entry("w7", false));
        let doomed = hidden_close_plan(false, &mut state, &live(&["w7"]));
        assert!(doomed.is_empty(), "closed a mirror on a host nobody hid");
        assert_eq!(state.workspaces.len(), 1, "map must survive untouched");
    }

    #[test]
    fn hiding_closes_live_mirrors_and_keeps_tombstones() {
        let mut state = HostState::default();
        state.workspaces.insert("R1".into(), ws_entry("w7", false));
        state.workspaces.insert("R2".into(), ws_entry("w8", true)); // user closed it
        state.workspaces.insert("R3".into(), ws_entry("w9", false)); // already gone
        let doomed = hidden_close_plan(true, &mut state, &live(&["w7", "w8"]));
        assert_eq!(
            doomed,
            vec!["w7".to_string()],
            "only the live, non-tombstoned one"
        );
        // the tombstone survives, or `show` resurrects a mirror the user closed
        assert!(state.workspaces.contains_key("R2"));
        assert!(!state.workspaces.contains_key("R1"));
        assert!(!state.workspaces.contains_key("R3"));
    }
    use super::*;

    fn ssh_host() -> HostConfig {
        HostConfig {
            name: "vps".into(),
            target: "vps".into(),
            kind: crate::config::HostKind::Ssh,
            docker_bin: "docker".into(),
            prefix: "vps".into(),
            remote_bin: None,
            relay_bin: None,
            session: None,
            always_control: true,
            max_cols: None,
            max_rows: None,
            api_transport: crate::config::ApiTransport::Auto,
        }
    }

    #[test]
    fn status_suspects_protected_and_explicit_zero() {
        let state: HostState = serde_json::from_value(json!({"workspaces":{"r":{"localId":"w"}}, "tabs":{"t":{"localId":"t"}}})).unwrap();
        let mut snap: Snapshot = serde_json::from_value(json!({"panes":[
            {"pane_id":"p1","workspace_id":"w","tab_id":"t", "cwd":"/tmp/.mirror-pane", "foreground_cwd":"/tmp/.mirror-pane"},
            {"pane_id":"p2","workspace_id":"w","tab_id":"t", "cwd":"/tmp/.mirror-pane", "foreground_cwd":"/tmp/.mirror-pane"}
        ]})).unwrap();
        let line = orphan_status(&snap, &state, |_| false);
        assert!(line.contains("suspects: 2"));
        assert!(line.contains("protected: 0"));
        snap.agents.push(serde_json::from_value(json!({"pane_id":"p1", "agent":"codex"})).unwrap());
        snap.panes[1].foreground_cwd = Some("/home/adopted".into());
        let line = orphan_status(&snap, &state, |_| false);
        assert!(line.contains("suspects: 0"));
        assert!(line.contains("agent"));
        assert!(line.contains("adopted/unknown cwd"));
        snap.panes.pop();
        let line = orphan_status(&snap, &state, |_| false);
        assert!(line.contains("suspects: 0"));
        assert!(line.contains("last workspace pane"));
        assert!(line.contains("last mapped-tab pane"));
        snap.panes.clear();
        assert!(orphan_status(&snap, &state, |_| false).contains("suspects: 0 []; protected: 0 []"));
    }

    fn leaf(pane_id: &str) -> LayoutNode {
        LayoutNode::Pane {
            pane_id: Some(pane_id.into()),
            label: None,
        }
    }

    fn split(direction: &str, ratio: f64, first: LayoutNode, second: LayoutNode) -> LayoutNode {
        LayoutNode::Split {
            direction: direction.into(),
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    /// The fallback path, used only for a pane `layout_sync::plan_placements`
    /// won't place (its remote sibling is a whole subtree, which `pane.split`
    /// can't wrap). Ratio deliberately not reported: the shape-preserving
    /// placements carry it, and here it would describe a different split.
    #[test]
    fn locate_in_layout_reports_direction_and_siblings() {
        let tree = split("right", 0.3, leaf("p1"), leaf("p2"));
        let (dir, sibs) = locate_in_layout(&tree, "p2").unwrap();
        assert_eq!(dir, "right");
        assert_eq!(sibs, vec!["p1".to_string()]);

        // nested: p3 is under a "down" split inside the "right" split's second
        // branch, so the nearest sibling is p2, not p1
        let tree = split(
            "right",
            0.3,
            leaf("p1"),
            split("down", 0.4, leaf("p2"), leaf("p3")),
        );
        let (dir, sibs) = locate_in_layout(&tree, "p3").unwrap();
        assert_eq!(dir, "down");
        assert_eq!(sibs, vec!["p2".to_string()]);

        assert!(locate_in_layout(&tree, "nope").is_none());
    }

    #[test]
    fn converge_lock_excludes_a_second_holder_per_host() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-mirror-converge-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first = try_converge_lock(&dir, "windows").unwrap();
        assert!(first.is_some());
        assert!(try_converge_lock(&dir, "windows").unwrap().is_none());
        // other hosts are independent
        assert!(try_converge_lock(&dir, "hermes").unwrap().is_some());
        drop(first);
        assert!(try_converge_lock(&dir, "windows").unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mirror-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lock_subprocess_helper() {
        let Ok(dir) = std::env::var("MIRROR_TEST_LOCK_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        if std::env::var("MIRROR_TEST_LOCK_ROLE").unwrap() == "holder" {
            let _held = try_converge_lock(&dir, "vps").unwrap().unwrap();
            std::fs::write(dir.join("ready"), b"ready").unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
            while !dir.join("exit").exists() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            // Deliberately bypass File::drop: OS must release the process lock.
            std::process::exit(0);
        } else {
            assert!(try_converge_lock(&dir, "vps").unwrap().is_none());
            let before = std::time::Instant::now();
            let api =
                ApiClient::mock(|method, _| panic!("contended converge must not call {method}"));
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(converge(&mock_deps(&dir, api)))
                .unwrap();
            assert!(before.elapsed() >= CONVERGE_LOCK_WAIT);
            assert!(before.elapsed() < CONVERGE_LOCK_WAIT + std::time::Duration::from_secs(5));
            assert!(!dir.join("vps-map.json").exists());
        }
    }

    #[test]
    fn lock_real_process_contention_and_holder_death() {
        let dir = test_dir("process");
        let spawn = |role| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "mirror::tests::lock_subprocess_helper",
                    "--nocapture",
                ])
                .env("MIRROR_TEST_LOCK_DIR", &dir)
                .env("MIRROR_TEST_LOCK_ROLE", role)
                .spawn()
                .unwrap()
        };
        let mut holder = spawn("holder");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !dir.join("ready").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(spawn("contender").wait().unwrap().success());
        std::fs::write(dir.join("exit"), b"exit").unwrap();
        assert!(holder.wait().unwrap().success());
        assert!(try_converge_lock(&dir, "vps").unwrap().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn mock_deps(dir: &std::path::Path, api: ApiClient) -> ConvergeDeps {
        ConvergeDeps {
            orphan_gc: false,
            local: api.clone(),
            remote: api,
            host: ssh_host(),
            state_dir: dir.into(),
            log: Logger::new(dir, false),
            close_remote_on_local_close: false,
            closes: Default::default(),
        }
    }

    #[tokio::test]
    async fn converge_noncontention_error_has_no_effects() {
        let dir = test_dir("fail-closed");
        let before = br#"{"panes":{"r":{"localId":"p","seq":0}}}"#;
        std::fs::write(dir.join("vps-map.json"), before).unwrap();
        std::fs::create_dir(converge_lock_path(&dir, "vps")).unwrap();
        let api = ApiClient::mock(|method, _| panic!("unexpected pane/server effect: {method}"));
        let deps = mock_deps(&dir, api);
        converge(&deps).await.unwrap();
        assert_eq!(std::fs::read(dir.join("vps-map.json")).unwrap(), before);
        let log = std::fs::read_to_string(dir.join("daemon.log")).unwrap();
        assert!(log.contains("skipping this pass"));
        assert!(log.contains("vps.converge.lock"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn complete_converge_holds_lock_during_mock_snapshot() {
        let dir = test_dir("integration");
        let check_dir = dir.clone();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        let api = ApiClient::mock(move |method, _| {
            assert!(
                try_converge_lock(&check_dir, "vps").unwrap().is_none(),
                "converge must hold lock before any RPC"
            );
            assert_eq!(method, "session.snapshot");
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({"snapshot": {}}))
        });
        converge(&mock_deps(&dir, api)).await.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(try_converge_lock(&dir, "vps").unwrap().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn tombstoned(local_id: &str) -> PaneEntry {
        PaneEntry {
            local_id: local_id.into(),
            tombstone: Some(true),
            seq: 0,
            reported: None,
        }
    }

    /// A locally-closed (tombstoned) pane must not survive into the tree a tab
    /// rebuild applies: layout.apply creates a real local pane per leaf, and a
    /// tombstoned one would be left as a dead shell no streamer ever claims.
    #[test]
    fn prune_closed_drops_tombstoned_panes_and_collapses_splits() {
        let tree = split(
            "right",
            0.3,
            leaf("p1"),
            split("down", 0.4, leaf("p2"), leaf("p3")),
        );

        // untracked and live panes survive untouched
        let mut panes: BTreeMap<String, PaneEntry> = BTreeMap::new();
        panes.insert(
            "p1".into(),
            PaneEntry {
                local_id: "l1".into(),
                tombstone: None,
                seq: 0,
                reported: None,
            },
        );
        let mut ids = Vec::new();
        walk_pane_ids(&prune_closed(&tree, &panes).unwrap(), &mut ids);
        assert_eq!(
            ids,
            vec!["p1".to_string(), "p2".to_string(), "p3".to_string()]
        );

        // a tombstoned leaf disappears and its split collapses to the sibling
        panes.insert("p2".into(), tombstoned("l2"));
        let pruned = prune_closed(&tree, &panes).unwrap();
        let mut ids = Vec::new();
        walk_pane_ids(&pruned, &mut ids);
        assert_eq!(ids, vec!["p1".to_string(), "p3".to_string()]);
        // the surviving outer split keeps its geometry
        let LayoutNode::Split {
            direction, ratio, ..
        } = &pruned
        else {
            panic!("outer split should survive");
        };
        assert_eq!(direction, "right");
        assert_eq!(*ratio, 0.3);

        // every pane tombstoned → None: the whole tab's mirror was closed
        panes.insert("p1".into(), tombstoned("l1"));
        panes.insert("p3".into(), tombstoned("l3"));
        assert!(prune_closed(&tree, &panes).is_none());
    }

    /// Characterization test: the ssh pane argv is a cross-process contract.
    ///
    /// The daemon spawns `herdr-mirror pane ...` as a separate process, and
    /// `count_streamers` (daemon.rs) identifies a host's live streamers by
    /// string-matching `--ctl-path` in that argv. Nothing else pins the shape,
    /// so a change here silently breaks mirror healing on upgrade: streamers
    /// started by the old binary carry the old argv, the new daemon fails to
    /// match them, concludes they died, and re-execs over live panes.
    ///
    /// If this test fails, that is the question to answer — not a prompt to
    /// update the expected value.
    #[cfg(unix)]
    #[test]
    fn ssh_pane_argv_is_stable() {
        let state_dir = std::path::Path::new("/state");
        let cmd = cmd_for_pane(&ssh_host(), state_dir, &HashMap::new());
        let argv = cmd("w1:p1");
        assert_eq!(
            argv[1..],
            [
                "pane",
                "vps",
                "w1:p1",
                // no --remote-bin: auto (PATH then ~/.local/bin/herdr)
                "--always-control",
                "--ctl-path",
                "/state/vps.ctl",
            ]
        );
        // argv[0] is the resolved exe path, which varies by install
        assert!(
            argv[0].ends_with("herdr-mirror") || argv[0].contains("herdr_mirror"),
            "{}",
            argv[0]
        );
    }

    /// When remote_bin is set, it must appear on the argv (cross-process contract
    /// with the pane parser) rather than being re-resolved by the streamer.
    #[cfg(unix)]
    #[test]
    fn ssh_pane_argv_carries_explicit_remote_bin() {
        let mut host = ssh_host();
        host.remote_bin = Some("/opt/herdr".into());
        let cmd = cmd_for_pane(&host, std::path::Path::new("/state"), &HashMap::new());
        let argv = cmd("w1:p1");
        assert_eq!(
            argv[1..],
            [
                "pane",
                "vps",
                "w1:p1",
                "--remote-bin",
                "/opt/herdr",
                "--always-control",
                "--ctl-path",
                "/state/vps.ctl",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn ssh_pane_argv_carries_remote_session() {
        let mut host = ssh_host();
        host.session = Some("work".into());
        let cmd = cmd_for_pane(&host, std::path::Path::new("/state"), &HashMap::new());
        let argv = cmd("w1:p1");
        assert_eq!(
            argv[1..],
            [
                "pane",
                "vps",
                "w1:p1",
                "--session",
                "work",
                "--always-control",
                "--ctl-path",
                "/state/vps.ctl",
            ]
        );
        let parsed = crate::pane::parse_args(&argv[2..]).expect("pane must parse daemon argv");
        assert_eq!(parsed.session.as_deref(), Some("work"));
    }

    /// Docker hosts append their flags *after* the ssh-shaped prefix, so the
    /// two argv layouts share a stable head and only diverge at the tail.
    #[test]
    fn docker_pane_argv_carries_container_and_no_identity_token() {
        let mut host = ssh_host();
        host.name = "token".into();
        host.target = "/Users/n/proj".into();
        host.kind = crate::config::HostKind::DockerFolder("/Users/n/proj".into());
        host.docker_bin = "/usr/local/bin/docker".into();
        let cmd = cmd_for_pane(&host, std::path::Path::new("/state"), &HashMap::new());
        let argv = cmd("w1:p1");
        assert_eq!(
            argv[1..],
            [
                "pane",
                "/Users/n/proj",
                "w1:p1",
                "--always-control",
                // no identity token at all: healing asks herdr per pane
                "--container-folder",
                "/Users/n/proj",
                "--docker-bin",
                "/usr/local/bin/docker",
            ]
        );
    }

    /// The argv the daemon emits must round-trip through the pane process's
    /// own parser — they are separate processes, so nothing else checks this.
    #[test]
    fn docker_argv_round_trips_through_pane_parser() {
        let mut host = ssh_host();
        host.kind = crate::config::HostKind::DockerContainer("crazy_ride".into());
        // deliberately NOT the default "docker": parse_args defaults to the
        // same value, so a fixture using the default would still pass if
        // cmd_for_pane stopped emitting --docker-bin. Users who need an
        // absolute path (GUI-launched daemons without /usr/local/bin on PATH)
        // would then silently get "cannot run docker".
        host.docker_bin = "/usr/local/bin/docker".into();
        let cmd = cmd_for_pane(&host, std::path::Path::new("/state"), &HashMap::new());
        let argv = cmd("w1:p1");
        let parsed = crate::pane::parse_args(&argv[2..]).expect("pane must parse daemon argv");
        assert_eq!(parsed.pane_target, "w1:p1");
        assert_eq!(parsed.ctl_path, None, "docker panes carry no ctl path");
        let ct = parsed
            .container
            .expect("container must survive the argv round trip");
        assert_eq!(
            ct.kind,
            crate::config::HostKind::DockerContainer("crazy_ride".into())
        );
        assert_eq!(
            ct.docker_bin, "/usr/local/bin/docker",
            "--docker-bin must round-trip"
        );
    }

    /// always_control is the only conditional flag; its absence must not
    /// disturb the position of --ctl-path.
    #[cfg(unix)]
    #[test]
    fn ssh_pane_argv_without_always_control() {
        let mut host = ssh_host();
        host.always_control = false;
        let cmd = cmd_for_pane(&host, std::path::Path::new("/state"), &HashMap::new());
        let argv = cmd("w1:p1");
        assert_eq!(
            argv[1..],
            ["pane", "vps", "w1:p1", "--ctl-path", "/state/vps.ctl"]
        );
    }

    #[test]
    fn windows_pane_argv_uses_windows_transport_without_ctl_path() {
        let mut host = ssh_host();
        host.name = "windows".into();
        host.kind = crate::config::HostKind::Windows;
        host.target = "windows".into();
        host.session = Some("team".into());
        host.remote_bin = Some(r"C:\tools\herdr.exe".into());
        host.relay_bin = Some(r"C:\tools\herdr-mirror.exe".into());
        let argv = cmd_for_pane(&host, std::path::Path::new("/state"), &HashMap::new())("w1:p1");
        // Same argv on both platforms: a Windows host is never multiplexed,
        // whichever OS the client runs on.
        assert_eq!(
            argv[1..],
            [
                "pane",
                "windows",
                "w1:p1",
                "--remote-bin",
                r"C:\tools\herdr.exe",
                "--session",
                "team",
                "--always-control",
                "--windows-host",
            ]
        );
        let parsed = crate::pane::parse_args(&argv[2..]).unwrap();
        assert!(parsed.windows_host);
        assert_eq!(parsed.remote_bin.as_deref(), Some(r"C:\tools\herdr.exe"));
        assert_eq!(parsed.ctl_path, None);
    }

    /// An uncapped host's argv must not grow, and a capped one must round-trip
    /// through the same parser the daemon's child uses.
    #[test]
    fn size_caps_reach_the_streamer_argv() {
        let mut host = ssh_host();
        host.always_control = false;
        let uncapped =
            cmd_for_pane(&host, std::path::Path::new("/state"), &HashMap::new())("w1:p1");
        assert!(!uncapped
            .iter()
            .any(|a| a == "--max-cols" || a == "--max-rows"));

        host.max_cols = Some(212);
        host.max_rows = Some(58);
        let argv = cmd_for_pane(&host, std::path::Path::new("/state"), &HashMap::new())("w1:p1");
        let parsed = crate::pane::parse_args(&argv[2..]).expect("pane must parse daemon argv");
        assert_eq!(parsed.max_cols, Some(212));
        assert_eq!(parsed.max_rows, Some(58));
        // the caps are a ceiling on control only — the observe request is
        // still whatever --cols/--rows said (here: unset, so the defaults,
        // which #42 made a floor rather than an exact size)
        assert_eq!((parsed.cols, parsed.rows), (240, 72));
    }

    #[test]
    fn ws_label_two_way_rename() {
        // in sync → nothing
        assert_eq!(
            resolve_label(Some("pm"), "scratch", "pm: scratch", Some("scratch")),
            LabelAction::InSync
        );
        // remote renamed (history differs) → remote wins
        assert_eq!(
            resolve_label(Some("pm"), "runs", "pm: scratch", Some("scratch")),
            LabelAction::RestampLocal
        );
        // no history (pre-upgrade state file) → remote wins once
        assert_eq!(
            resolve_label(Some("pm"), "scratch", "pm: LLMs", None),
            LabelAction::RestampLocal
        );
        // user renamed locally, kept the prefix → push stripped name to remote
        assert_eq!(
            resolve_label(Some("pm"), "scratch", "pm: LLMs", Some("scratch")),
            LabelAction::PushRemote("LLMs".into())
        );
        // user renamed locally without prefix → push as-is
        assert_eq!(
            resolve_label(Some("pm"), "scratch", "LLM runs", Some("scratch")),
            LabelAction::PushRemote("LLM runs".into())
        );
        // degenerate: renamed to just the prefix-colon or whitespace → restamp
        assert_eq!(
            resolve_label(Some("pm"), "scratch", "pm:  ", Some("scratch")),
            LabelAction::RestampLocal
        );
    }

    /// Tabs carry the remote label verbatim, so the same resolution runs with no
    /// prefix. The third case is the bug this exists to prevent: a local rename
    /// used to be invisible, so converge restamped it back from the remote.
    #[test]
    fn tab_label_two_way_rename() {
        // in sync → nothing
        assert_eq!(
            resolve_label(None, "logs", "logs", Some("logs")),
            LabelAction::InSync
        );
        // remote renamed since we last stamped → remote wins
        assert_eq!(
            resolve_label(None, "build", "logs", Some("logs")),
            LabelAction::RestampLocal
        );
        // user renamed the mirror tab, remote unchanged → push it to the remote
        assert_eq!(
            resolve_label(None, "logs", "deploys", Some("logs")),
            LabelAction::PushRemote("deploys".into())
        );
        // no history (tab mapped by an older mirror) → remote wins once
        assert_eq!(
            resolve_label(None, "logs", "deploys", None),
            LabelAction::RestampLocal
        );
        // renamed to whitespace → restamp rather than push an empty label
        assert_eq!(
            resolve_label(None, "logs", "   ", Some("logs")),
            LabelAction::RestampLocal
        );
        // a never-named remote tab reports its position as its label, so a local
        // rename of one still has to push rather than restamp
        assert_eq!(
            resolve_label(None, "2", "notes", Some("2")),
            LabelAction::PushRemote("notes".into())
        );
    }

    /// The `pane_agent_status_changed` event (herdr app/api.rs) must deserialize
    /// into AgentInfo cleanly, or flush_status would fall back to a default (no
    /// agent) and wrongly retract the mirror's agent. Note the event carries the
    /// title as `title`, which lands in its own field and still reaches the
    /// reported title slot through `effective_title`.
    #[test]
    fn agent_status_event_parses_and_keeps_title() {
        let data = json!({
            "pane_id": "w1:p1",
            "workspace_id": "w1",
            "agent_status": "working",
            "agent": "claude",
            "title": "fix the bug",
            "display_agent": "Claude",
            "custom_status": null,
            "state_labels": { "branch": "main" }
        });
        let info: AgentInfo = serde_json::from_value(data).unwrap();
        assert_eq!(info.agent.as_deref(), Some("claude"));
        assert_eq!(info.agent_status.as_deref(), Some("working"));
        assert_eq!(info.display_agent.as_deref(), Some("Claude"));
        assert_eq!(info.title.as_deref(), Some("fix the bug"));
        assert_eq!(info.effective_title(), Some("fix the bug"));
        assert!(info.has_agent());
    }

    /// A named agent that also carries a pane title must parse: with `title`
    /// aliased onto `name` it was a duplicate-field error, which fails the
    /// whole snapshot parse (`agents` is a Vec) and wedges the host.
    #[test]
    fn agent_with_both_name_and_title_parses() {
        let agents: Vec<AgentInfo> = serde_json::from_value(json!([
            { "pane_id": "w1:p1", "agent": "claude", "name": "l2-r3", "title": "fix the bug" },
            { "pane_id": "w1:p2", "agent": "codex" },
        ]))
        .expect("a named agent with a pane title must not fail the snapshot parse");
        assert_eq!(agents.len(), 2);
        // an explicit name still wins over the pane title
        assert_eq!(agents[0].effective_title(), Some("l2-r3"));
        assert_eq!(agents[0].title.as_deref(), Some("fix the bug"));
        assert_eq!(agents[1].effective_title(), None);
    }

    /// A remote agent with a user-given name keeps showing it; only an
    /// unnamed agent falls back to the remote's live terminal title, so a
    /// mirrored agent's current task is visible instead of always blank
    /// (the reported gap: oldmac reports `terminal_title_stripped` on every
    /// agent, but the mirror only ever forwarded `name`, which most agents
    /// never set).
    #[test]
    fn effective_title_prefers_name_falls_back_to_terminal_title() {
        let named = AgentInfo {
            name: Some("l2-r3".into()),
            terminal_title_stripped: Some("实现论文引用图数据层".into()),
            ..Default::default()
        };
        assert_eq!(named.effective_title(), Some("l2-r3"));

        let unnamed = AgentInfo {
            name: None,
            terminal_title_stripped: Some("实现论文引用图数据层".into()),
            terminal_title: Some("✳ 实现论文引用图数据层".into()),
            ..Default::default()
        };
        assert_eq!(unnamed.effective_title(), Some("实现论文引用图数据层"));

        let stripped_missing = AgentInfo {
            name: None,
            terminal_title_stripped: None,
            terminal_title: Some("✳ working".into()),
            ..Default::default()
        };
        assert_eq!(stripped_missing.effective_title(), Some("✳ working"));

        let bare = AgentInfo {
            name: None,
            ..Default::default()
        };
        assert_eq!(bare.effective_title(), None);
    }

    // simulate herdr's move_workspace(source, insert_index) on an id list
    fn apply_move(order: &mut Vec<String>, ws: &str, insert_index: usize) {
        let src = order.iter().position(|w| w == ws).unwrap();
        let target_idx = if src < insert_index {
            insert_index - 1
        } else {
            insert_index
        };
        let item = order.remove(src);
        order.insert(target_idx, item);
    }

    fn ranked(items: &[(&str, usize)]) -> Vec<(String, usize)> {
        items.iter().map(|(s, r)| (s.to_string(), *r)).collect()
    }

    #[test]
    fn regroup_groups_and_only_moves_mirrors() {
        // rank 0 = local, 1 = work, 2 = vps; interleaved current order
        let current = ranked(&[("L1", 0), ("W1", 1), ("V1", 2), ("L2", 0), ("W2", 1)]);
        let moves = plan_regroup(&current);
        // never move a local
        let rank_of = |id: &str| current.iter().find(|(i, _)| i == id).unwrap().1;
        for (id, _) in &moves {
            assert!(rank_of(id) > 0, "planner moved a local row: {id}");
        }
        // applying the plan yields the grouped order
        let mut order: Vec<String> = current.iter().map(|(id, _)| id.clone()).collect();
        for (ws, idx) in &moves {
            apply_move(&mut order, ws, *idx);
        }
        assert_eq!(order, vec!["L1", "L2", "W1", "W2", "V1"]);
    }

    #[test]
    fn regroup_is_noop_when_already_grouped() {
        let current = ranked(&[("L1", 0), ("L2", 0), ("W1", 1), ("W2", 1), ("V1", 2)]);
        assert!(plan_regroup(&current).is_empty());
    }

    #[test]
    fn regroup_new_mirror_slots_into_its_block() {
        // a new work workspace appended at the bottom (the reported bug)
        let current = ranked(&[("L1", 0), ("W1", 1), ("V1", 2), ("W2", 1)]);
        let mut order: Vec<String> = current.iter().map(|(id, _)| id.clone()).collect();
        for (ws, idx) in plan_regroup(&current) {
            apply_move(&mut order, &ws, idx);
        }
        assert_eq!(order, vec!["L1", "W1", "W2", "V1"]); // W2 rises above V1
    }

    #[test]
    fn ws_rank_classifies_by_prefix() {
        let prefixes = vec!["work".to_string(), "vps".to_string()];
        assert_eq!(ws_rank("work: slice", &prefixes), 1);
        assert_eq!(ws_rank("vps: ~", &prefixes), 2);
        assert_eq!(ws_rank("utopia", &prefixes), 0); // local
    }

    /// An agent-exit event carries no agent + "unknown" status → has_agent()
    /// false, so push_pane_status retracts (the intended release path).
    #[test]
    fn agent_exit_event_reads_as_no_agent() {
        let data = json!({
            "pane_id": "w1:p1",
            "workspace_id": "w1",
            "agent_status": "unknown",
            "agent": null,
            "display_agent": null,
            "custom_status": null,
            "state_labels": null
        });
        let info: AgentInfo = serde_json::from_value(data).unwrap();
        assert!(!info.has_agent());
    }

    // --- M1 rev: copy found by streamer key, orphan sweep ---

    use super::{collect_streamer_keys, plan_orphan_sweep, streamer_key, StreamerKeys};
    use serde_json::{json, Value};

    fn keys(pairs: &[(&str, &[&str])]) -> StreamerKeys {
        pairs.iter().map(|(r, v)| (r.to_string(), v.iter().map(|s| s.to_string()).collect())).collect()
    }

    fn mirror_pane(id: &str) -> PaneInfo {
        lpane(id, Some("/s/.mirror-pane"))
    }

    #[test]
    fn streamer_key_reads_target_and_rid_from_argv() {
        let a = |v: &[&str]| v.iter().map(|s| json!(s)).collect::<Vec<Value>>();
        assert_eq!(
            streamer_key(&a(&["/x/herdr-mirror", "pane", "win", "wN:p1", "--remote-bin", "h"])),
            Some(("win".into(), "wN:p1".into()))
        );
        assert_eq!(streamer_key(&a(&["/x/herdr-mirror", "status"])), None);
        assert_eq!(streamer_key(&a(&["ssh", "pane", "win", "wN:p1"])), None);
    }

    #[test]
    fn stale_stored_id_still_closes_the_live_copy_found_by_key() {
        // the observed case: stored id absent locally, copy alive under another id
        let st = state_with("wN:p1", pane_entry("w6E:p1P6", false));
        let sn = snap(vec![mirror_pane("w6E:p1Q0")], &["w6E:p1Q0"]);
        let k = keys(&[("wN:p1", &["w6E:p1Q0"])]);
        assert_eq!(plan_pane_reap("wN:p1", &st, &sn, &k), PaneReap::Close("w6E:p1Q0".into()));
    }

    #[test]
    fn key_with_zero_hits_is_absent_and_several_hits_refuse() {
        let st = state_with("wN:p1", pane_entry("w6E:p1P6", false));
        let sn = snap(vec![mirror_pane("w6E:p1Q0"), mirror_pane("w6E:p1Q1")], &[]);
        assert_eq!(plan_pane_reap("wN:p1", &st, &sn, &no_keys()), PaneReap::Absent);
        let k = keys(&[("wN:p1", &["w6E:p1Q0", "w6E:p1Q1"])]);
        assert!(matches!(plan_pane_reap("wN:p1", &st, &sn, &k), PaneReap::Refuse(_)));
    }

    #[test]
    fn keyed_pane_that_is_another_entrys_copy_is_refused() {
        let mut st = state_with("wN:p1", pane_entry("w6E:p1P6", false));
        st.panes.insert("wN:p2".into(), pane_entry("w6E:p1Q0", false));
        let sn = snap(vec![mirror_pane("w6E:p1Q0")], &[]);
        let k = keys(&[("wN:p1", &["w6E:p1Q0"])]);
        assert!(matches!(plan_pane_reap("wN:p1", &st, &sn, &k), PaneReap::Refuse(_)));
    }

    #[test]
    fn orphan_is_closed_only_on_the_second_pass() {
        let st = HostState::default();
        let sn = snap(vec![mirror_pane("w6E:p1Q0")], &[]);
        let k = keys(&[("wN:p9", &["w6E:p1Q0"])]);
        let remote: HashSet<&str> = HashSet::new();
        let (close1, seen1) = plan_orphan_sweep(&st, &sn, &remote, &k, &Default::default());
        assert!(close1.is_empty());
        assert!(seen1.contains("w6E:p1Q0"));
        let (close2, _) = plan_orphan_sweep(&st, &sn, &remote, &k, &seen1);
        assert_eq!(close2, vec![("wN:p9".to_string(), "w6E:p1Q0".to_string())]);
    }

    #[test]
    fn sweep_spares_mapped_remote_alive_foreign_agent_and_non_mirror() {
        let prev: std::collections::BTreeSet<String> =
            ["m1", "m2", "m3", "m4", "m5"].iter().map(|s| s.to_string()).collect();
        // m1: rid mapped; m2: rid alive on the remote; m3: local id mapped under
        // another rid; m4: live non-mirror agent; m5: not a mirror pane at all
        let mut st = state_with("r1", pane_entry("zz", false));
        st.panes.insert("rX".into(), pane_entry("m3", false));
        let sn = snap(
            vec![mirror_pane("m1"), mirror_pane("m2"), mirror_pane("m3"), lpane("m4", Some("/home/u")), lpane("m5", Some("/home/u"))],
            &["m4"],
        );
        let k = keys(&[("r1", &["m1"]), ("r2", &["m2"]), ("r3", &["m3"]), ("r4", &["m4"]), ("r5", &["m5"])]);
        let remote: HashSet<&str> = ["r2"].into_iter().collect();
        let (close, _) = plan_orphan_sweep(&st, &sn, &remote, &k, &prev);
        assert!(close.is_empty(), "{close:?}");
    }

    #[tokio::test]
    async fn collect_keys_via_process_info_on_fake_socket() {
        let d = tmpdir("keys");
        let (api, seen) = fake_herdr_with(&d, |m, p| {
            assert_eq!(m, "pane.process_info");
            let rid = if p["pane_id"] == "w6E:p1Q0" { "wN:p1" } else { "wN:p2" };
            Ok(json!({"process_info": {"foreground_processes": [
                {"argv": ["/b/herdr-mirror", "pane", if p["pane_id"] == "other" { "elsewhere" } else { "win" }, rid]}
            ]}}))
        })
        .await;
        let sn = snap(vec![mirror_pane("w6E:p1Q0"), mirror_pane("w6E:p1Q1"), lpane("w6E:p1Q2", Some("/home/u"))], &[]);
        let k = collect_streamer_keys(&api, &sn, "win").await;
        assert_eq!(k.get("wN:p1").unwrap(), &vec!["w6E:p1Q0".to_string()]);
        assert_eq!(k.get("wN:p2").unwrap(), &vec!["w6E:p1Q1".to_string()]);
        // the non-mirror pane is never even probed
        assert_eq!(seen.lock().unwrap().len(), 2);
        let k2 = collect_streamer_keys(&api, &sn, "other-host").await;
        assert!(k2.is_empty());
    }

    #[tokio::test]
    async fn end_to_end_stale_id_orphan_gets_closed_over_fake_socket() {
        let d = tmpdir("e2e");
        let (api, seen) = fake_herdr_with(&d, |m, p| match m {
            "pane.process_info" => Ok(json!({"process_info": {"foreground_processes": [
                {"argv": ["/b/herdr-mirror", "pane", "win", "wN:p1"]}]}})),
            "pane.close" => {
                assert_eq!(p["pane_id"], "w6E:p1Q0");
                Ok(json!({}))
            }
            other => Err(format!("unexpected {other}")),
        })
        .await;
        let log = Logger::new(&d, false);
        let st = state_with("wN:p1", pane_entry("w6E:p1P6", false));
        let sn = snap(vec![mirror_pane("w6E:p1Q0")], &[]);
        let k = collect_streamer_keys(&api, &sn, "win").await;
        let plan = plan_pane_reap("wN:p1", &st, &sn, &k);
        assert!(execute_pane_reap(&api, &log, "wN:p1", "w6E:p1P6", plan, |_| {}).await);
        let s = seen.lock().unwrap();
        assert_eq!(s.last().unwrap().0, "pane.close");
        assert_eq!(s.last().unwrap().1["pane_id"], "w6E:p1Q0");
    }

    /// Fake herdr socket with a per-request handler (Err => error response).
    async fn fake_herdr_with(
        dir: &std::path::Path,
        handler: impl Fn(&str, &Value) -> std::result::Result<Value, String> + Send + Sync + 'static,
    ) -> (ApiClient, Arc<Mutex<Vec<(String, Value)>>>) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let path = dir.join("fake2.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (seen2, handler) = (seen.clone(), Arc::new(handler));
        tokio::spawn(async move {
            loop {
                let Ok((st, _)) = listener.accept().await else { return };
                let (seen, handler) = (seen2.clone(), handler.clone());
                tokio::spawn(async move {
                    let (r, mut w) = st.into_split();
                    if let Ok(Some(l)) = BufReader::new(r).lines().next_line().await {
                        let v: Value = serde_json::from_str(&l).unwrap();
                        let (m, p) = (v["method"].as_str().unwrap().to_string(), v["params"].clone());
                        seen.lock().unwrap().push((m.clone(), p.clone()));
                        let out = match handler(&m, &p) {
                            Ok(r) => json!({"id": v["id"], "result": r}),
                            Err(e) => json!({"id": v["id"], "error": {"message": e}}),
                        };
                        let _ = w.write_all((out.to_string() + "\n").as_bytes()).await;
                    }
                });
            }
        });
        (ApiClient::connect_unchecked(&path), seen)
    }
}
