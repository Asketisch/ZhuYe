//! Actor 模型 —— 统一「谁做了什么」的语义层(WP10 收编 ReflectDesktop
//! `app-core/src/actor.rs`)。
//!
//! 桌面端注释自述「不重构核心 Task schema,把 Actor 作为语义层类型编码进
//! `Task.metadata.actor`」—— 那是受 submodule 只读约束的权宜。归位到
//! zhuye-task 后,本模块成为 `Task.metadata.actor` 的**官方编解码**,
//! squad / activity 审计等消费方不再各自拼 JSON。
//!
//! ## 约定
//!
//! - `actor_id` 是稳定字符串:
//!   - `"user"` —— 本地人类用户(单用户假定);
//!   - `"system"` —— 系统级动作(cron / autopilot / 崩溃恢复);
//!   - `"<role>@<team>"` —— 团队成员(与 [`TeamMemberSpec::agent_id`]
//!     同形,经 [`crate::team::parse_agent_id`] 拆分)。
//! - [`ActorType`] 粗粒度(Human / Agent / System),供 UI 图标 / 颜色;
//! - [`ActorKind`] 细粒度(User / Lead / Member / System),供 UI 徽标。
//!
//! 序列化 `camelCase`(与 TS 端对齐);不改 `Task` schema。

use serde::{Deserialize, Serialize};

use crate::model::TeamMemberSpec;

/// 粗粒度 actor 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActorType {
    /// 人类用户。
    Human,
    /// Agent(主 agent / subagent / team lead / team member)。
    Agent,
    /// 系统(cron / autopilot / 内部调度)。
    System,
}

impl ActorType {
    /// `"human"` / `"agent"` / `"system"`。
    pub fn as_str(self) -> &'static str {
        match self {
            ActorType::Human => "human",
            ActorType::Agent => "agent",
            ActorType::System => "system",
        }
    }
}

impl std::fmt::Display for ActorType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 细粒度 actor 角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActorKind {
    /// 本地用户。
    User,
    /// 团队领导(`team-lead@<team>`)。
    Lead,
    /// 团队成员(非 lead 的 agent)。
    Member,
    /// 系统级动作。
    System,
}

impl ActorKind {
    /// `"user"` / `"lead"` / `"member"` / `"system"`。
    pub fn as_str(self) -> &'static str {
        match self {
            ActorKind::User => "user",
            ActorKind::Lead => "lead",
            ActorKind::Member => "member",
            ActorKind::System => "system",
        }
    }
}

impl std::fmt::Display for ActorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 统一 actor —— 「谁做了什么」的答案。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Actor {
    /// 粗粒度类型。
    pub actor_type: ActorType,
    /// 稳定 id(`"user"` / `"system"` / `"<role>@<team>"`)。
    pub actor_id: String,
    /// 细粒度角色。
    pub kind: ActorKind,
    /// 人类可读名(可选;UI 优先用它,回退 `actor_id`)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// 所属 team 名(仅 agent 成员有)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_name: Option<String>,
}

impl Actor {
    /// 本地用户 actor。
    pub fn user() -> Self {
        Self {
            actor_type: ActorType::Human,
            actor_id: "user".into(),
            kind: ActorKind::User,
            display_name: Some("You".into()),
            team_name: None,
        }
    }

    /// 系统 actor。
    pub fn system() -> Self {
        Self {
            actor_type: ActorType::System,
            actor_id: "system".into(),
            kind: ActorKind::System,
            display_name: Some("System".into()),
            team_name: None,
        }
    }

    /// 从 team 名 + role 构造 agent actor。
    ///
    /// `role == "team-lead"` → [`ActorKind::Lead`];其它 → [`ActorKind::Member`]。
    pub fn agent(team: &str, role: &str) -> Self {
        let actor_id = format!("{role}@{team}");
        let kind = if role == "team-lead" {
            ActorKind::Lead
        } else {
            ActorKind::Member
        };
        Self {
            actor_type: ActorType::Agent,
            actor_id,
            kind,
            display_name: Some(role.to_string()),
            team_name: Some(team.to_string()),
        }
    }

    /// 用 `<role>@<team>` 字符串反解析(与 [`crate::team::parse_agent_id`]
    /// 拆分口径一致);`"user"` / `"system"` 特殊值;其它未知 id 保守归
    /// System,避免误导。
    pub fn from_agent_id(agent_id: &str) -> Self {
        match crate::team::parse_agent_id(agent_id) {
            Some((role, team)) => Self::agent(team, role),
            None => {
                if agent_id == "user" {
                    Self::user()
                } else if agent_id == "system" {
                    Self::system()
                } else {
                    Self {
                        actor_type: ActorType::System,
                        actor_id: agent_id.to_string(),
                        kind: ActorKind::System,
                        display_name: Some(agent_id.to_string()),
                        team_name: None,
                    }
                }
            }
        }
    }

    /// 编码为可塞进 `Task.metadata` 的 JSON(`{"actor": {...}}`,外层包
    /// 一层 `actor` key 避免与其它 metadata 字段冲突)。
    pub fn encode_metadata(&self) -> serde_json::Value {
        serde_json::json!({ "actor": self })
    }

    /// 从 `Task.metadata`(或任意 JSON)反解 actor。
    ///
    /// 接受 `{"actor": {...}}`(本模块写入形态)与裸 actor object(宽容);
    /// 都解不出 → `None`。
    pub fn decode_metadata(metadata: &serde_json::Value) -> Option<Actor> {
        if let Some(actor_val) = metadata.get("actor") {
            serde_json::from_value(actor_val.clone()).ok()
        } else {
            serde_json::from_value(metadata.clone()).ok()
        }
    }

    /// 把 actor 合并进既有 metadata(只改 `actor` 子键,**不**用空对象
    /// 覆盖其它键 —— squad / activity 之外的工具 / 集成写入的字段要保留)。
    /// `actor = None` 移除 `actor` 子键。非 object 的 metadata 以空 map 起步。
    pub fn merge_into_metadata(
        actor: Option<&Actor>,
        metadata: &serde_json::Value,
    ) -> serde_json::Value {
        let mut map = match metadata {
            serde_json::Value::Object(m) => m.clone(),
            _ => serde_json::Map::new(),
        };
        match actor {
            Some(a) => {
                map.insert(
                    "actor".into(),
                    serde_json::to_value(a).unwrap_or(serde_json::Value::Null),
                );
            }
            None => {
                map.remove("actor");
            }
        }
        serde_json::Value::Object(map)
    }
}

impl Default for Actor {
    fn default() -> Self {
        Self::user()
    }
}

/// 把 [`TeamMemberSpec`] 转成 actor;`name` 非空时优先作 `display_name`。
pub fn actor_from_team_member(member: &TeamMemberSpec) -> Actor {
    let mut actor = Actor::from_agent_id(&member.agent_id);
    if !member.name.is_empty() {
        actor.display_name = Some(member.name.clone());
    }
    actor
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_actor_serializes_camelcase() {
        let v = serde_json::to_value(Actor::user()).unwrap();
        assert_eq!(v["actorType"], "human");
        assert_eq!(v["actorId"], "user");
        assert_eq!(v["kind"], "user");
        assert_eq!(v["displayName"], "You");
    }

    #[test]
    fn system_actor_kind_is_system() {
        let a = Actor::system();
        assert_eq!(a.actor_type, ActorType::System);
        assert_eq!(a.kind, ActorKind::System);
        assert_eq!(a.actor_id, "system");
    }

    #[test]
    fn lead_and_member_detected_from_role() {
        let lead = Actor::agent("rocket", "team-lead");
        assert_eq!(lead.kind, ActorKind::Lead);
        assert_eq!(lead.actor_id, "team-lead@rocket");
        assert_eq!(lead.team_name.as_deref(), Some("rocket"));
        let member = Actor::agent("rocket", "architect");
        assert_eq!(member.kind, ActorKind::Member);
        assert_eq!(member.actor_id, "architect@rocket");
    }

    #[test]
    fn from_agent_id_roundtrip_and_specials() {
        assert_eq!(
            Actor::from_agent_id("builder@rocket").kind,
            ActorKind::Member
        );
        assert_eq!(
            Actor::from_agent_id("team-lead@alpha").kind,
            ActorKind::Lead
        );
        assert_eq!(Actor::from_agent_id("user").kind, ActorKind::User);
        assert_eq!(Actor::from_agent_id("system").kind, ActorKind::System);
        // 未知裸 id 保守归 System。
        assert_eq!(Actor::from_agent_id("mystery").kind, ActorKind::System);
    }

    #[test]
    fn encode_decode_metadata_roundtrip() {
        let a = Actor::agent("rocket", "team-lead");
        let decoded = Actor::decode_metadata(&a.encode_metadata()).unwrap();
        assert_eq!(a, decoded);
    }

    #[test]
    fn decode_metadata_tolerates_bare_object_and_rejects_garbage() {
        let bare = json!({"actorType": "agent", "actorId": "architect@rocket", "kind": "member"});
        assert_eq!(
            Actor::decode_metadata(&bare).unwrap().actor_id,
            "architect@rocket"
        );
        assert!(Actor::decode_metadata(&json!({"foo": "bar"})).is_none());
        assert!(Actor::decode_metadata(&json!(null)).is_none());
    }

    #[test]
    fn merge_into_metadata_preserves_other_keys() {
        // 既有键必须保留;None 只移除 actor 子键。
        let base = json!({"source": "importer", "priority": 5});
        let merged = Actor::merge_into_metadata(Some(&Actor::agent("t", "builder")), &base);
        assert_eq!(merged["source"], "importer");
        assert_eq!(merged["priority"], 5);
        assert!(merged.get("actor").is_some());
        let cleared = Actor::merge_into_metadata(None, &merged);
        assert_eq!(cleared["source"], "importer");
        assert!(cleared.get("actor").is_none());
        // 非 object 起步为空 map。
        let from_null = Actor::merge_into_metadata(Some(&Actor::user()), &json!(null));
        assert!(from_null.is_object() && from_null.get("actor").is_some());
    }

    #[test]
    fn actor_from_team_member_uses_name_as_display() {
        let m = TeamMemberSpec {
            agent_id: "team-lead@rocket".into(),
            name: "Rocket Lead".into(),
            role: "team-lead".into(),
            model: None,
            system_prompt: String::new(),
            allowed_tools: vec![],
            color: None,
            joined_at: std::time::SystemTime::UNIX_EPOCH,
            session_id: None,
            subscriptions: vec![],
        };
        let a = actor_from_team_member(&m);
        assert_eq!(a.display_name.as_deref(), Some("Rocket Lead"));
        assert_eq!(a.kind, ActorKind::Lead);
    }

    #[test]
    fn display_matches_serde_and_default_is_user() {
        assert_eq!(ActorType::Human.to_string(), "human");
        assert_eq!(ActorKind::Lead.to_string(), "lead");
        assert_eq!(Actor::default().kind, ActorKind::User);
    }
}
