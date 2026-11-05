//! squad 测试(移植 ReflectDesktop squad.rs 的 13 例 + created_at 还原回归)。

use super::*;
use crate::actor::ActorKind;
use crate::store::InMemoryTaskStore;
use crate::team_store::InMemoryTeamStore;

fn make_manager() -> SquadManager {
    let task_store = Arc::new(InMemoryTaskStore::default());
    let team_store = Arc::new(InMemoryTeamStore::default());
    SquadManager::new(Arc::new(TaskManager::new(task_store, team_store)))
}

fn sample_spec(name: &str) -> SquadSpec {
    SquadSpec {
        name: name.into(),
        description: Some("test squad".into()),
        leader_actor: Actor::agent(name, "team-lead"),
        members: vec![SquadMember {
            actor: Actor::agent(name, "builder"),
            role: "builder".into(),
            model: None,
            system_prompt: String::new(),
            allowed_tools: vec!["Write".into(), "Edit".into()],
        }],
        created_at_ms: 1000,
    }
}

async fn create_task(m: &SquadManager, list: &str, meta: serde_json::Value) -> Task {
    m.task_manager
        .create_task(
            &list.to_string(),
            "t1".into(),
            "desc".into(),
            None,
            None,
            meta,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn create_and_list_squad() {
    let m = make_manager();
    m.create_squad(sample_spec("rocket")).await.unwrap();
    let list = m.list_squads().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "rocket");
}

#[tokio::test]
async fn get_squad_returns_leader_and_members() {
    let m = make_manager();
    m.create_squad(sample_spec("alpha")).await.unwrap();
    let s = m.get_squad("alpha").await.unwrap();
    assert_eq!(s.leader_actor.kind, ActorKind::Lead);
    assert_eq!(s.leader_actor.actor_id, "team-lead@alpha");
    assert_eq!(s.members.len(), 1);
    assert_eq!(s.members[0].role, "builder");
}

#[tokio::test]
async fn get_squad_not_found_is_typed() {
    let m = make_manager();
    assert!(matches!(
        m.get_squad("ghost").await,
        Err(SquadError::NotFound { name }) if name == "ghost"
    ));
}

#[tokio::test]
async fn delete_squad_removes_it() {
    let m = make_manager();
    m.create_squad(sample_spec("beta")).await.unwrap();
    m.delete_squad("beta").await.unwrap();
    assert!(m.get_squad("beta").await.is_err());
}

#[tokio::test]
async fn invalid_name_rejected() {
    let m = make_manager();
    let mut bad = sample_spec("x");
    bad.name = "Bad Name".into();
    assert!(matches!(
        m.create_squad(bad).await,
        Err(SquadError::Invalid { .. })
    ));
}

#[tokio::test]
async fn roundtrip_team_file_preserves_members_and_created_at() {
    let spec = sample_spec("gamma");
    let team = SquadManager::to_team_file(&spec);
    assert_eq!(team.lead_agent_id, "team-lead@gamma");
    // 1 leader + 1 成员。
    assert_eq!(team.members.len(), 2);
    // 回归:created_at 不再写 UNIX_EPOCH 丢信息(桌面端旧行为)。
    assert_eq!(
        team.created_at
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
        1000
    );
    let back = SquadManager::from_team_file(&team);
    assert_eq!(back.name, "gamma");
    assert_eq!(back.members.len(), 1);
    assert_eq!(back.members[0].role, "builder");
    assert_eq!(back.created_at_ms, 1000);
}

#[tokio::test]
async fn create_squad_keeps_leader_display_name() {
    let m = make_manager();
    let mut spec = sample_spec("named");
    spec.leader_actor.display_name = Some("Captain".into());
    m.create_squad(spec).await.unwrap();
    let s = m.get_squad("named").await.unwrap();
    // actor_id 被强制对齐,但 display_name 保留。
    assert_eq!(s.leader_actor.actor_id, "team-lead@named");
    assert_eq!(s.leader_actor.display_name.as_deref(), Some("Captain"));
}

#[tokio::test]
async fn delegate_next_claims_pending_task() {
    let m = make_manager();
    m.create_squad(sample_spec("delta")).await.unwrap();
    let task = create_task(&m, "delta", serde_json::json!({})).await;
    let claimed = m.delegate_next("delta", "team-lead@delta").await.unwrap();
    assert_eq!(claimed.map(|t| t.id), Some(task.id));
}

#[tokio::test]
async fn delegate_next_none_when_no_tasks() {
    let m = make_manager();
    m.create_squad(sample_spec("epsilon")).await.unwrap();
    assert!(
        m.delegate_next("epsilon", "team-lead@epsilon")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn assign_task_sets_owner_and_metadata() {
    let m = make_manager();
    m.create_squad(sample_spec("zeta")).await.unwrap();
    let task = create_task(&m, "zeta", serde_json::json!({})).await;
    let updated = m
        .assign_task("zeta", task.id, Some("builder@zeta".into()))
        .await
        .unwrap();
    assert_eq!(updated.owner.as_deref(), Some("builder@zeta"));
    let actor = Actor::decode_metadata(&updated.metadata).unwrap();
    assert_eq!(actor.actor_id, "builder@zeta");
    assert_eq!(actor.kind, ActorKind::Member);
}

#[tokio::test]
async fn assign_task_clears_assignee_with_none() {
    let m = make_manager();
    m.create_squad(sample_spec("eta")).await.unwrap();
    let task = create_task(&m, "eta", serde_json::json!({})).await;
    m.assign_task("eta", task.id, Some("builder@eta".into()))
        .await
        .unwrap();
    let cleared = m.assign_task("eta", task.id, None).await.unwrap();
    assert!(cleared.owner.is_none());
    assert!(Actor::decode_metadata(&cleared.metadata).is_none());
}

#[tokio::test]
async fn assign_task_preserves_existing_metadata_on_clear() {
    // 回归:清空 assignee 不应擦掉既有 metadata 的其它键。
    let m = make_manager();
    m.create_squad(sample_spec("theta")).await.unwrap();
    let task = create_task(
        &m,
        "theta",
        serde_json::json!({ "source": "importer", "priority": 5 }),
    )
    .await;
    let assigned = m
        .assign_task("theta", task.id, Some("builder@theta".into()))
        .await
        .unwrap();
    assert_eq!(assigned.metadata["source"], "importer");
    assert_eq!(assigned.metadata["priority"], 5);
    assert!(assigned.metadata.get("actor").is_some());
    let cleared = m.assign_task("theta", task.id, None).await.unwrap();
    assert_eq!(cleared.metadata["source"], "importer");
    assert_eq!(cleared.metadata["priority"], 5);
    assert!(cleared.metadata.get("actor").is_none());
}

#[test]
fn squad_error_display_and_name_validation() {
    assert!(
        SquadError::NotFound { name: "x".into() }
            .to_string()
            .contains('x')
    );
    assert!(SquadManager::validate_name("rocket").is_ok());
    assert!(SquadManager::validate_name("a-b_c").is_ok());
    assert!(SquadManager::validate_name("").is_err());
    assert!(SquadManager::validate_name("Has Space").is_err());
    assert!(SquadManager::validate_name("UPPER").is_err());
}

struct SlowTaskStore(InMemoryTaskStore);

#[async_trait::async_trait]
impl crate::store::TaskStore for SlowTaskStore {
    fn dir_for(&self, list: &crate::model::ListId) -> std::path::PathBuf {
        self.0.dir_for(list)
    }
    async fn save(&self, list: &crate::model::ListId, task: &Task) -> Result<(), TaskError> {
        tokio::time::sleep(Duration::from_millis(30)).await;
        self.0.save(list, task).await
    }
    async fn load(&self, list: &crate::model::ListId, id: TaskId) -> Result<Task, TaskError> {
        self.0.load(list, id).await
    }
    async fn delete(&self, list: &crate::model::ListId, id: TaskId) -> Result<(), TaskError> {
        self.0.delete(list, id).await
    }
    async fn list(&self, list: &crate::model::ListId) -> Result<Vec<Task>, TaskError> {
        self.0.list(list).await
    }
    async fn next_id(&self, list: &crate::model::ListId) -> Result<TaskId, TaskError> {
        self.0.next_id(list).await
    }
}

#[tokio::test]
async fn assignment_and_concurrent_update_preserve_both_changes() {
    let manager = Arc::new(TaskManager::new(
        Arc::new(SlowTaskStore(InMemoryTaskStore::default())),
        Arc::new(InMemoryTeamStore::default()),
    ));
    let squad = SquadManager::new(manager.clone());
    let task = create_task(&squad, "parallel", serde_json::json!({"keep": "value"})).await;
    let list = "parallel".to_string();
    let assignment = squad.assign_task(&list, task.id, Some("builder@parallel".into()));
    let update = manager.update_task(
        &list,
        task.id,
        crate::manager::TaskPatch {
            subject: Some("新标题".into()),
            ..Default::default()
        },
    );
    let (assigned, updated) = tokio::join!(assignment, update);
    assigned.unwrap();
    updated.unwrap();
    let result = manager.get_task(&list, task.id, false).await.unwrap();
    assert_eq!(result.subject, "新标题");
    assert_eq!(result.owner.as_deref(), Some("builder@parallel"));
    assert_eq!(result.metadata["keep"], "value");
    assert_eq!(result.metadata["actor"]["actorId"], "builder@parallel");
}

#[tokio::test]
async fn claim_and_concurrent_update_preserve_both_changes() {
    let manager = Arc::new(TaskManager::new(
        Arc::new(SlowTaskStore(InMemoryTaskStore::default())),
        Arc::new(InMemoryTeamStore::default()),
    ));
    let squad = SquadManager::new(manager.clone());
    let task = create_task(&squad, "parallel", serde_json::json!({})).await;
    let list = "parallel".to_string();
    let claim = squad.delegate_next(&list, "team-lead@parallel");
    let update = manager.update_task(
        &list,
        task.id,
        crate::manager::TaskPatch {
            subject: Some("新标题".into()),
            ..Default::default()
        },
    );
    let (claimed, updated) = tokio::join!(claim, update);
    assert!(claimed.unwrap().is_some());
    updated.unwrap();
    let result = manager.get_task(&list, task.id, false).await.unwrap();
    assert_eq!(result.subject, "新标题");
    assert_eq!(result.status, crate::model::TaskStatus::InProgress);
    assert_eq!(result.claimed_by.as_deref(), Some("team-lead@parallel"));
}

#[tokio::test]
async fn delete_during_update_does_not_resurrect_task() {
    let manager = Arc::new(TaskManager::new(
        Arc::new(SlowTaskStore(InMemoryTaskStore::default())),
        Arc::new(InMemoryTeamStore::default()),
    ));
    let squad = SquadManager::new(manager.clone());
    let task = create_task(&squad, "parallel", serde_json::json!({})).await;
    let list = "parallel".to_string();
    let update = manager.update_task(
        &list,
        task.id,
        crate::manager::TaskPatch {
            subject: Some("更新".into()),
            ..Default::default()
        },
    );
    let delete = async {
        tokio::time::sleep(Duration::from_millis(5)).await;
        manager.delete_task(&list, task.id).await
    };
    let (updated, deleted) = tokio::join!(update, delete);
    updated.unwrap();
    deleted.unwrap();
    assert!(matches!(
        manager.get_task(&list, task.id, false).await,
        Err(TaskError::NotFound { .. })
    ));
}
