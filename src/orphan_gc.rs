//! Fail-closed daemon-only orphan shell collection.
use super::*;
use crate::state::OrphanCandidate;
use crate::util::err;

#[cfg(test)]
#[path = "orphan_gc_tests.rs"]
mod tests;

const MIN_AGE: u64 = 30;
const MAX_GAP: u64 = 180;

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?.as_str()
}
fn owned_path(v: &Value, key: &str, marker: &std::path::Path) -> bool {
    text(v, key)
        .and_then(|p| std::fs::canonicalize(p).ok())
        .as_deref()
        == Some(marker)
}
fn bare(p: &Value, marker: &std::path::Path) -> bool {
    p.get("agent").is_none_or(Value::is_null)
        && text(p, "agent_status") == Some("unknown")
        && p.get("revision").and_then(Value::as_u64).is_some()
        && owned_path(p, "cwd", marker)
        && owned_path(p, "foreground_cwd", marker)
}
fn identity(p: &Value, marker: &std::path::Path, now: u64) -> Option<OrphanCandidate> {
    if !bare(p, marker) {
        return None;
    }
    let terminal_id = text(p, "terminal_id")?;
    if terminal_id.is_empty() {
        return None;
    }
    Some(OrphanCandidate {
        terminal_id: terminal_id.into(),
        workspace_id: text(p, "workspace_id")?.into(),
        tab_id: text(p, "tab_id")?.into(),
        marker: marker.to_str()?.into(),
        first_seen: now,
        last_seen: now,
    })
}
fn same(a: &OrphanCandidate, b: &OrphanCandidate) -> bool {
    a.terminal_id == b.terminal_id
        && a.workspace_id == b.workspace_id
        && a.tab_id == b.tab_id
        && a.marker == b.marker
}
fn idle(info: &Value, id: &str, marker: &std::path::Path) -> bool {
    let Some(pid) = info
        .get("shell_pid")
        .and_then(Value::as_u64)
        .filter(|p| *p > 0)
    else {
        return false;
    };
    let Some(procs) = info.get("foreground_processes").and_then(Value::as_array) else {
        return false;
    };
    text(info, "pane_id") == Some(id)
        && info
            .get("foreground_process_group_id")
            .and_then(Value::as_u64)
            == Some(pid)
        && procs.len() == 1
        && procs[0].get("pid").and_then(Value::as_u64) == Some(pid)
        && text(&procs[0], "name").is_some_and(|s| !s.is_empty())
        && procs[0]
            .get("argv")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
        && owned_path(&procs[0], "cwd", marker)
}

struct Inventory {
    panes: Vec<Value>,
    agents: Vec<Value>,
    /// Panes missing terminal_id / agent_status / malformed agent: unknown, never collected.
    partial: HashSet<String>,
}
async fn inventory(api: &ApiClient) -> Result<Inventory> {
    let response = api.request("session.snapshot", json!({})).await?;
    let s = &response["snapshot"];
    let array = |key: &str| {
        s.get(key)
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| err(format!("GC incomplete {key}")))
    };
    let workspaces = array("workspaces")?;
    let tabs = array("tabs")?;
    let panes = array("panes")?;
    let agents = array("agents")?;
    let layouts = array("layouts")?;
    let unique = |rows: &[Value], key: &str| -> bool {
        let ids: Option<HashSet<&str>> = rows.iter().map(|v| text(v, key)).collect();
        ids.is_some_and(|ids| ids.len() == rows.len())
    };
    if !unique(&workspaces, "workspace_id")
        || !unique(&tabs, "tab_id")
        || !unique(&panes, "pane_id")
        || !unique(&agents, "pane_id")
        || !unique(&layouts, "tab_id")
    {
        return Err(err("GC duplicate/missing IDs"));
    }
    for w in &workspaces {
        let id = text(w, "workspace_id");
        if w["pane_count"].as_u64()
            != Some(
                panes
                    .iter()
                    .filter(|p| text(p, "workspace_id") == id)
                    .count() as u64,
            )
            || w["tab_count"].as_u64()
                != Some(
                    tabs.iter()
                        .filter(|t| text(t, "workspace_id") == id)
                        .count() as u64,
                )
        {
            return Err(err("GC workspace counts"));
        }
    }
    for t in &tabs {
        let id = text(t, "tab_id");
        if !workspaces
            .iter()
            .any(|w| text(w, "workspace_id") == text(t, "workspace_id"))
            || t["pane_count"].as_u64()
                != Some(panes.iter().filter(|p| text(p, "tab_id") == id).count() as u64)
        {
            return Err(err("GC tab counts"));
        }
        let layout = layouts
            .iter()
            .find(|l| text(l, "tab_id") == id)
            .ok_or_else(|| err("GC missing layout"))?;
        let lp = layout["panes"]
            .as_array()
            .ok_or_else(|| err("GC partial layout"))?;
        let expected: HashSet<_> = panes
            .iter()
            .filter(|p| text(p, "tab_id") == id)
            .filter_map(|p| text(p, "pane_id"))
            .collect();
        if !unique(lp, "pane_id")
            || lp
                .iter()
                .filter_map(|p| text(p, "pane_id"))
                .collect::<HashSet<_>>()
                != expected
        {
            return Err(err("GC layout mismatch"));
        }
    }
    let mut partial = HashSet::new();
    for p in &panes {
        if !tabs.iter().any(|t| {
            text(t, "tab_id") == text(p, "tab_id")
                && text(t, "workspace_id") == text(p, "workspace_id")
        }) {
            return Err(err("GC partial pane"));
        }
        if text(p, "terminal_id").is_none()
            || text(p, "agent_status").is_none()
            || !p.get("agent").is_none_or(|a| a.is_null() || a.is_string())
        {
            // Missing `agent` key = no agent (like null). Partial = unknown => spared.
            partial.insert(text(p, "pane_id").unwrap_or_default().to_string());
        }
        // A live agent must also occur in agents[]. Missing rows are unknown.
        if p["agent"].is_string()
            && !agents
                .iter()
                .any(|a| text(a, "pane_id") == text(p, "pane_id"))
        {
            return Err(err("GC incomplete agents"));
        }
    }
    if agents.iter().any(|a| {
        !panes
            .iter()
            .any(|p| text(p, "pane_id") == text(a, "pane_id"))
    }) {
        return Err(err("GC foreign agent"));
    }
    Ok(Inventory {
        panes,
        agents,
        partial,
    })
}
/// (ssh target, remote pane id) a streamer pane was launched for: its argv is
/// `[bin, "pane", <target>, <rid>, ...]` (mirror::cmd_for_pane). Durable,
/// readable through pane.process_info, and never touched by the sync.
pub(crate) fn argv_mark(info: &Value) -> Option<(String, String)> {
    let argv = info
        .get("foreground_processes")?
        .as_array()?
        .first()?
        .get("argv")?
        .as_array()?;
    let a = |i: usize| argv.get(i)?.as_str().map(String::from);
    (a(1)? == "pane").then_some(())?;
    Some((a(2)?, a(3)?)).filter(|(t, r)| !t.is_empty() && !r.is_empty())
}
/// Panes in the marker cwd, not mapped in `state`, whose streamer argv names
/// (`target`, rid) with rid unmapped. Candidates for adoption; fail-closed on
/// any read error is the caller's job (errors just skip the pane here).
async fn unmapped_marks(
    local: &ApiClient,
    inv: &Inventory,
    marker: &std::path::Path,
    target: &str,
    state: &HostState,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for p in &inv.panes {
        let Some(id) = text(p, "pane_id") else { continue };
        if !owned_path(p, "cwd", marker)
            || state.panes.values().any(|m| m.local_id == id)
            || !state
                .workspaces
                .values()
                .any(|w| !w.is_tombstoned() && Some(w.local_id.as_str()) == text(p, "workspace_id"))
        {
            continue;
        }
        let Ok(info) = local.pane_process_info(id).await else { continue };
        if let Some((t, rid)) = argv_mark(&info) {
            if t == target && !state.panes.contains_key(&rid) {
                out.push((id.to_string(), rid));
            }
        }
    }
    out
}
/// Panes spared as awaiting adoption: unmapped mark whose rid is in the remote
/// snapshot. With no remote client (gc-plan) or an unreadable remote, spare.
async fn awaiting_set(
    local: &ApiClient,
    inv: &Inventory,
    marker: &std::path::Path,
    target: &str,
    state: &HostState,
    remote: Option<&ApiClient>,
) -> HashSet<String> {
    let marks = unmapped_marks(local, inv, marker, target, state).await;
    if marks.is_empty() {
        return HashSet::new();
    }
    let rids: Option<HashSet<String>> = match remote {
        None => None,
        Some(r) => crate::mirror::fetch_snapshot(r)
            .await
            .ok()
            .map(|s| s.panes.into_iter().map(|p| p.pane_id).collect()),
    };
    marks
        .into_iter()
        .filter(|(_, rid)| rids.as_ref().is_none_or(|s| s.contains(rid)))
        .map(|(id, _)| id)
        .collect()
}
/// The live local copy already launched for (`target`, `rid`), if any: adopt it
/// rather than splitting a duplicate. None on any doubt, including API errors.
pub(super) async fn find_copy(
    local: &ApiClient,
    state_dir: &std::path::Path,
    target: &str,
    rid: &str,
    state: &HostState,
) -> Option<String> {
    let marker = std::fs::canonicalize(mirror_pane_cwd(state_dir)).ok()?;
    // not inventory(): its strict consistency checks can fail mid-reconcile
    let snap = local.request("session.snapshot", json!({})).await.ok()?;
    let panes = snap["snapshot"]["panes"].as_array()?.clone();
    let inv = Inventory { panes, agents: vec![], partial: Default::default() };
    unmapped_marks(local, &inv, &marker, target, state)
        .await
        .into_iter()
        .find(|(_, r)| r == rid)
        .map(|(id, _)| id)
}
fn eligible(inv: &Inventory, p: &Value, state: &HostState, awaiting: &HashSet<String>) -> bool {
    let Some(id) = text(p, "pane_id") else {
        return false;
    };
    if inv.partial.contains(id) {
        return false;
    }
    // A copy whose streamer names a remote pane that still exists but isn't
    // mapped yet is awaiting adoption by reconcile.
    if awaiting.contains(id) {
        return false;
    }
    state
        .workspaces
        .values()
        .any(|w| !w.is_tombstoned() && Some(w.local_id.as_str()) == text(p, "workspace_id"))
        && !state.panes.values().any(|m| m.local_id == id)
        && !inv.agents.iter().any(|a| text(a, "pane_id") == Some(id))
        && inv
            .panes
            .iter()
            .filter(|v| text(v, "workspace_id") == text(p, "workspace_id"))
            .count()
            > 1
        && (!(state
            .tabs
            .values()
            .any(|t| Some(t.local_id.as_str()) == text(p, "tab_id"))
            || state
                .workspaces
                .values()
                .any(|w| w.root_tab_local_id.as_deref() == text(p, "tab_id")))
            || inv
                .panes
                .iter()
                .filter(|v| text(v, "tab_id") == text(p, "tab_id"))
                .count()
                > 1)
}

pub(super) async fn collect(deps: &ConvergeDeps, state: &mut HostState) {
    if let Err(e) = run(deps, state).await {
        state.orphan_candidates.clear();
        deps.log.log(&format!("orphan GC skipped/reset: {e}"));
    }
}
async fn run(deps: &ConvergeDeps, state: &mut HostState) -> Result<()> {
    // Fresh inventory AFTER reconciliation closes/creates; never use its old snapshot.
    let inv = inventory(&deps.local).await?;
    let marker = std::fs::canonicalize(mirror_pane_cwd(&deps.state_dir))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let old = std::mem::take(&mut state.orphan_candidates);
    let mut closes = 0;
    let awaiting = awaiting_set(&deps.local, &inv, &marker, &deps.host.target, state, Some(&deps.remote)).await;
    for p in &inv.panes {
        if !eligible(&inv, p, state, &awaiting) {
            continue;
        }
        let id = text(p, "pane_id").unwrap();
        let Some(mut candidate) = identity(p, &marker, now) else {
            continue;
        };
        if !idle(&deps.local.pane_process_info(id).await?, id, &marker) {
            continue;
        }
        let confirmed = old
            .get(id)
            .filter(|o| same(o, &candidate) && now >= o.last_seen && now - o.last_seen <= MAX_GAP);
        if let Some(o) = confirmed {
            candidate.first_seen = o.first_seen;
        }
        state.orphan_candidates.insert(id.into(), candidate.clone());
        if confirmed.is_none() || now.saturating_sub(candidate.first_seen) < MIN_AGE || closes >= 10
        {
            continue;
        }
        // Recount before EACH close, including successful closes earlier this pass.
        let fresh = inventory(&deps.local).await?;
        let fresh_awaiting = awaiting_set(&deps.local, &fresh, &marker, &deps.host.target, state, Some(&deps.remote)).await;
        let Some(current) = fresh.panes.iter().find(|v| text(v, "pane_id") == Some(id)) else {
            state.orphan_candidates.remove(id);
            continue;
        };
        if !eligible(&fresh, current, state, &fresh_awaiting)
            || !identity(current, &marker, now).is_some_and(|c| same(&candidate, &c))
        {
            state.orphan_candidates.remove(id);
            continue;
        }
        let got = deps
            .local
            .request("pane.get", json!({"pane_id":id}))
            .await?;
        let pane = &got["pane"];
        if text(pane, "pane_id") != Some(id)
            || !identity(pane, &marker, now).is_some_and(|c| same(&candidate, &c))
            || !idle(&deps.local.pane_process_info(id).await?, id, &marker)
        {
            state.orphan_candidates.remove(id);
            continue;
        }
        // The server has no conditional close. An accepted millisecond TOCTOU
        // remains after these reads; only a positively idle, childless shell is targeted.
        mark_self_close(deps, id);
        deps.local
            .request("pane.close", json!({"pane_id":id}))
            .await?;
        closes += 1;
        state.orphan_candidates.remove(id);
    }
    Ok(())
}

/// Read-only dry run of the sweep: same `inventory`/`eligible`/`identity`/`idle`
/// as `run`, but it only holds `&ApiClient` and issues only `session.snapshot`
/// and `pane.process_info`. There is no close call in this function.
pub(super) async fn plan(
    local: &ApiClient,
    state_dir: &std::path::Path,
    host: &str,
    target: &str,
    state: &HostState,
) -> Vec<String> {
    let map = format!("{host}-map.json");
    let inv = match inventory(local).await {
        Ok(i) => i,
        Err(e) => return vec![format!("[{host}] ABORT: {e} (map={map})")],
    };
    let marker = match std::fs::canonicalize(mirror_pane_cwd(state_dir)) {
        Ok(m) => m,
        Err(e) => return vec![format!("[{host}] ABORT: marker dir: {e} (map={map})")],
    };
    // gc-plan never contacts the remote: an unmapped streamer mark is spared
    // without checking the remote still has that pane.
    let awaiting = awaiting_set(local, &inv, &marker, target, state, None).await;
    let mut out = Vec::new();
    for p in &inv.panes {
        let id = text(p, "pane_id").unwrap_or("?");
        if awaiting.contains(id) {
            out.push(format!("[{host}] SPARED-AWAITING-ADOPTION pane={id} (streamer names an unmapped remote pane) map={map}"));
            continue;
        }
        let s = |k: &str| text(p, k).unwrap_or("-");
        if inv.partial.contains(id) {
            out.push(format!(
                "[{host}] SPARED-PARTIAL pane={id} ws={} tab={} cwd={} agent_status={} map={map}",
                s("workspace_id"), s("tab_id"), s("cwd"), s("agent_status")
            ));
            continue;
        }
        if !eligible(&inv, p, state, &awaiting) || identity(p, &marker, 0).is_none() {
            continue;
        }
        let idle_now = match local.pane_process_info(id).await {
            Ok(i) => idle(&i, id, &marker),
            Err(_) => false,
        };
        out.push(format!(
            "[{host}] WOULD-CLOSE pane={id} ws={} tab={} cwd={} agent={} agent_status={} idle={idle_now} map={map} (daemon also needs age>={MIN_AGE}s + 2 sightings)",
            s("workspace_id"), s("tab_id"), s("cwd"),
            p.get("agent").and_then(Value::as_str).unwrap_or("none"),
            s("agent_status"),
        ));
    }
    if out.is_empty() {
        out.push(format!("[{host}] nothing to close (panes={})", inv.panes.len()));
    }
    out
}
