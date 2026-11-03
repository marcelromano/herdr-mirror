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
    p.get("agent").is_some_and(Value::is_null)
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
    for p in &panes {
        if !tabs.iter().any(|t| {
            text(t, "tab_id") == text(p, "tab_id")
                && text(t, "workspace_id") == text(p, "workspace_id")
        }) || text(p, "terminal_id").is_none()
            || text(p, "agent_status").is_none()
            || !p.get("agent").is_some_and(|a| a.is_null() || a.is_string())
        {
            return Err(err("GC partial pane"));
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
    Ok(Inventory { panes, agents })
}
fn eligible(inv: &Inventory, p: &Value, state: &HostState) -> bool {
    let Some(id) = text(p, "pane_id") else {
        return false;
    };
    // A copy marked with a rid the state doesn't know yet is awaiting adoption
    // by reconcile; one whose rid maps elsewhere is a duplicate (fair game).
    if crate::mirror::pane_rid(p).is_some_and(|r| !state.panes.contains_key(r)) {
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
    for p in &inv.panes {
        if !eligible(&inv, p, state) {
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
        let Some(current) = fresh.panes.iter().find(|v| text(v, "pane_id") == Some(id)) else {
            state.orphan_candidates.remove(id);
            continue;
        };
        if !eligible(&fresh, current, state)
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
