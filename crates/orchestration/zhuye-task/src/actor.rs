//! Actor 兼容层 —— 纯 Actor 类型已下沉 [`zhuye_protocol::actor`]
//! (WP12:telemetry 的 Activity 审计需要 Actor,而分层禁止
//! resources → orchestration 依赖)。本模块保留 `actor_from_team_member`
//! (它引用本 crate 的 `TeamMemberSpec`)并 re-export 协议类型,WB10 的
//! 公开 API(`zhuye_task::Actor` 等)不变。

use crate::model::TeamMemberSpec;

pub use zhuye_protocol::actor::{Actor, ActorKind, ActorType};

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
}
