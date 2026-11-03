use super::*;
use std::sync::{Arc, Mutex};

struct Fixture {
    deps: ConvergeDeps,
    state: HostState,
    snapshot: Arc<Mutex<Value>>,
    closed: Arc<Mutex<Vec<String>>>,
    mode: Arc<Mutex<String>>,
}
impl Fixture {
    fn new(count: usize) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "t1284-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join(".mirror-pane")).unwrap();
        let marker = std::fs::canonicalize(dir.join(".mirror-pane"))
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let panes: Vec<_> = (0..count).map(|i| json!({"pane_id":format!("p{i}"),"terminal_id":format!("term{i}"),"workspace_id":"w","tab_id":"t","cwd":marker,"foreground_cwd":marker,"agent":null,"agent_status":"unknown","revision":1})).collect();
        let snapshot = Arc::new(Mutex::new(
            json!({"workspaces":[{"workspace_id":"w","label":"vps: test","pane_count":count,"tab_count":1}],"tabs":[{"tab_id":"t","workspace_id":"w","pane_count":count}],"panes":panes,"agents":[],"layouts":[{"tab_id":"t","panes":panes.iter().map(|p| json!({"pane_id":p["pane_id"],"rect":{"width":80,"height":24}})).collect::<Vec<_>>()}]}),
        ));
        let closed = Arc::new(Mutex::new(Vec::new()));
        let mode = Arc::new(Mutex::new(String::new()));
        let (s, c, m) = (snapshot.clone(), closed.clone(), mode.clone());
        let api = ApiClient::mock(move |method, params| {
            let mut s = s.lock().unwrap();
            let mode = m.lock().unwrap().clone();
            let id = params["pane_id"].as_str().unwrap_or("");
            match method {
                "session.snapshot" => {
                    if mode == "fail" {
                        return Err(err("injected snapshot failure"));
                    }
                    Ok(json!({"snapshot":s.clone()}))
                }
                "pane.get" => {
                    let mut p = s["panes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|p| text(p, "pane_id") == Some(id))
                        .unwrap()
                        .clone();
                    match mode.as_str() {
                        "agent" => {
                            p["agent"] = json!("claude");
                            p["agent_status"] = json!("working");
                        }
                        "reuse" => p["terminal_id"] = json!("replacement"),
                        "move" => p["workspace_id"] = json!("other"),
                        "adopt" => p["foreground_cwd"] = json!("/not-owned"),
                        _ => (),
                    }
                    Ok(json!({"pane":p}))
                }
                "pane.process_info" => Ok(
                    json!({"process_info":{"pane_id":id,"shell_pid":42,"foreground_process_group_id":42,"foreground_processes":[{"pid":42,"name":"sh","argv":["sh"],"cwd":marker}]}}),
                ),
                "pane.close" => {
                    c.lock().unwrap().push(id.to_string());
                    s["panes"]
                        .as_array_mut()
                        .unwrap()
                        .retain(|p| text(p, "pane_id") != Some(id));
                    s["layouts"][0]["panes"]
                        .as_array_mut()
                        .unwrap()
                        .retain(|p| text(p, "pane_id") != Some(id));
                    let n = s["panes"].as_array().unwrap().len();
                    s["workspaces"][0]["pane_count"] = json!(n);
                    s["tabs"][0]["pane_count"] = json!(n);
                    Ok(json!({}))
                }
                _ => panic!("unexpected {method}"),
            }
        });
        let host = HostConfig {
            name: "vps".into(),
            target: "vps".into(),
            prefix: "vps".into(),
            kind: crate::config::HostKind::Ssh,
            docker_bin: "docker".into(),
            remote_bin: None,
            relay_bin: None,
            session: None,
            always_control: true,
            max_cols: None,
            max_rows: None,
            api_transport: crate::config::ApiTransport::Auto,
        };
        let deps = ConvergeDeps {
            orphan_gc: true,
            local: api,
            remote: ApiClient::mock(|method, _| {
                assert_eq!(method, "session.snapshot");
                Ok(json!({"snapshot":{}}))
            }),
            host,
            state_dir: dir.clone(),
            log: crate::util::Logger::new(&dir, false),
            close_remote_on_local_close: false,
            closes: Default::default(),
        };
        let state = serde_json::from_value(
            json!({"workspaces":{"rw":{"localId":"w"}},"prev_remote_ids":["rw"]}),
        )
        .unwrap();
        Self {
            deps,
            state,
            snapshot,
            closed,
            mode,
        }
    }
    async fn plan(&mut self) {
        collect(&self.deps, &mut self.state).await;
    }
    fn age(&mut self) {
        for c in self.state.orphan_candidates.values_mut() {
            c.first_seen -= MIN_AGE;
        }
    }
    fn count(&self) -> usize {
        self.closed.lock().unwrap().len()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.deps.state_dir);
    }
}

#[tokio::test]
async fn two_passes_limit_and_last_pane() {
    let mut f = Fixture::new(15);
    f.plan().await;
    assert_eq!(f.count(), 0);
    f.age();
    f.plan().await;
    assert_eq!(f.count(), 10);
    f.plan().await;
    assert_eq!(f.count(), 14);
}
#[tokio::test]
async fn partial_agents_and_failure_reset_confirmation() {
    for mode in ["missing", "fail"] {
        let mut f = Fixture::new(2);
        f.plan().await;
        f.age();
        if mode == "missing" {
            f.snapshot
                .lock()
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove("agents");
        } else {
            *f.mode.lock().unwrap() = "fail".into();
        }
        f.plan().await;
        assert_eq!(f.count(), 0);
        assert!(f.state.orphan_candidates.is_empty());
        if mode == "fail" {
            *f.mode.lock().unwrap() = String::new();
            f.plan().await;
            assert_eq!(f.count(), 0);
        }
    }
}
#[tokio::test]
async fn final_reads_reject_agent_reuse_move_and_adoption() {
    for mode in ["agent", "reuse", "move", "adopt"] {
        let mut f = Fixture::new(2);
        f.plan().await;
        f.age();
        *f.mode.lock().unwrap() = mode.into();
        f.plan().await;
        assert_eq!(f.count(), 0, "{mode}");
        assert!(f.state.orphan_candidates.is_empty());
    }
}
#[tokio::test]
async fn fresh_inventory_after_support_removed() {
    let mut f = Fixture::new(2);
    f.plan().await;
    f.age();
    f.deps
        .local
        .request("pane.close", json!({"pane_id":"p1"}))
        .await
        .unwrap();
    f.plan().await;
    assert_eq!(f.count(), 1); // only the injected support close
}
#[tokio::test]
async fn full_converge_daemon_collects_once_does_not() {
    for daemon in [false, true] {
        let mut f = Fixture::new(2);
        f.plan().await;
        f.age();
        f.deps.orphan_gc = daemon;
        save_state(&f.deps.state_dir, &f.deps.host.name, &f.state).unwrap();
        let state = converge(&f.deps).await.unwrap();
        assert_eq!(f.count(), usize::from(daemon));
        assert!(state.panes.values().all(|p| !p.is_tombstoned()));
        let ids: HashSet<String> = f.closed.lock().unwrap().iter().cloned().collect();
        let mut tracker = f.deps.closes.lock().unwrap();
        for id in &ids {
            tracker.note_close_event(id);
        }
        assert!(
            tracker.take_user_closed(&ids).is_empty(),
            "GC events must not become tombstones"
        );
    }
}
#[tokio::test]
async fn section_two_removes_support_before_gc() {
    let mut f = Fixture::new(2);
    f.plan().await;
    f.age();
    f.state.panes.insert(
        "absent-remote".into(),
        PaneEntry {
            local_id: "p1".into(),
            tombstone: None,
            seq: 0,
            reported: None,
        },
    );
    save_state(&f.deps.state_dir, &f.deps.host.name, &f.state).unwrap();
    converge(&f.deps).await.unwrap();
    assert_eq!(*f.closed.lock().unwrap(), vec!["p1".to_string()]);
}
#[tokio::test]
async fn snapshot_identity_change_restarts_age() {
    for key in ["terminal_id", "tab_id", "workspace_id"] {
        let mut f = Fixture::new(2);
        f.plan().await;
        f.age();
        if key == "terminal_id" {
            f.snapshot.lock().unwrap()["panes"][0][key] = json!("new");
        } else {
            // coherent move of the whole tab/workspace, not a partial snapshot
            let mut s = f.snapshot.lock().unwrap();
            if key == "workspace_id" {
                s["workspaces"][0][key] = json!("new");
                s["tabs"][0][key] = json!("new");
            } else {
                s["tabs"][0][key] = json!("new");
                s["layouts"][0][key] = json!("new");
            }
            for p in s["panes"].as_array_mut().unwrap() {
                p[key] = json!("new");
            }
        }
        f.plan().await;
        if key == "terminal_id" {
            // p1 may close, but p0 must not inherit its old confirmation
            assert!(!f.closed.lock().unwrap().contains(&"p0".to_string()));
        } else {
            assert_eq!(f.count(), 0);
        }
    }
}
#[tokio::test]
async fn minimum_age_and_unknown_ownership_protect_shells() {
    let mut f = Fixture::new(2);
    f.plan().await;
    f.plan().await;
    assert_eq!(f.count(), 0);
    f.age();
    f.snapshot.lock().unwrap()["panes"][0]["foreground_cwd"] = Value::Null;
    f.state.panes.insert(
        "rp".into(),
        PaneEntry {
            local_id: "p1".into(),
            tombstone: None,
            seq: 0,
            reported: None,
        },
    );
    f.plan().await;
    assert_eq!(f.count(), 0);
    assert!(f.state.orphan_candidates.is_empty());
}
#[tokio::test]
async fn mapped_tab_and_inconsistent_inventory_are_protected() {
    let mut f = Fixture::new(2);
    f.plan().await;
    f.age();
    f.snapshot.lock().unwrap()["workspaces"][0]["pane_count"] = json!(99);
    f.plan().await;
    assert_eq!(f.count(), 0);
    assert!(f.state.orphan_candidates.is_empty());
    let mut f = Fixture::new(2);
    {
        let mut s = f.snapshot.lock().unwrap();
        s["panes"][1]["tab_id"] = json!("t2");
        s["tabs"] = json!([{"tab_id":"t","workspace_id":"w","pane_count":1},{"tab_id":"t2","workspace_id":"w","pane_count":1}]);
        s["workspaces"][0]["tab_count"] = json!(2);
        s["layouts"] = json!([{"tab_id":"t","panes":[{"pane_id":"p0"}]},{"tab_id":"t2","panes":[{"pane_id":"p1"}]}]);
    }
    f.state.tabs.insert(
        "rt".into(),
        crate::state::TabEntry {
            local_id: "t".into(),
            last_remote_label: None,
        },
    );
    f.plan().await;
    f.age();
    f.plan().await;
    assert!(!f.closed.lock().unwrap().contains(&"p0".to_string()));
}
#[test]
fn unknown_process_is_not_idle() {
    let p = std::path::Path::new("/absent");
    assert!(!idle(&json!({}), "p", p));
    assert!(!idle(
        &json!({"pane_id":"p","shell_pid":1,"foreground_process_group_id":1,"foreground_processes":[]}),
        "p",
        p
    ));
}

#[test]
fn rid_label_roundtrip_and_unmapped_mark_not_collected() {
    let p = json!({"label": crate::mirror::rid_label("w7:p15")});
    assert_eq!(crate::mirror::pane_rid(&p), Some("w7:p15"));
    assert_eq!(crate::mirror::pane_rid(&json!({"label":"mirror:"})), None);
    assert_eq!(crate::mirror::pane_rid(&json!({"label":"x"})), None);
    let inv = Inventory { panes: vec![], agents: vec![], partial: Default::default() };
    let st = HostState::default();
    let marked = json!({"pane_id":"p9","workspace_id":"w","label":"mirror:w7:p15"});
    assert!(!eligible(&inv, &marked, &st));
}
#[tokio::test]
async fn partial_panes_are_spared_without_aborting_sweep() {
    // Shape of a real local snapshot, anonymized: plain shell panes omit the
    // `agent` key entirely (agent_status "unknown"), other fields present.
    let mut f = Fixture::new(3);
    {
        let mut s = f.snapshot.lock().unwrap();
        for i in 0..3 {
            let p = s["panes"][i].as_object_mut().unwrap();
            p.insert("terminal_title".into(), json!("TITLE"));
            p.insert("terminal_title_stripped".into(), json!("TITLE"));
            p.insert("focused".into(), json!(false));
            p.insert("scroll".into(), json!({"offset":0}));
        }
        s["panes"][1].as_object_mut().unwrap().remove("agent");
        s["panes"][2].as_object_mut().unwrap().remove("terminal_id");
    }
    f.plan().await;
    f.age();
    f.plan().await;
    let closed = f.closed.lock().unwrap().clone();
    assert_eq!(closed, vec!["p0".to_string(), "p1".to_string()]);
    assert!(!f.state.orphan_candidates.contains_key("p2"));
}
