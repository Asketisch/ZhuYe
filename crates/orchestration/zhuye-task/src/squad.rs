//! Squad + Leader 委派 —— 多 agent 小组的语义层(WP10 收编 ReflectDesktop
//! `app-core/src/squad.rs`)。
//!
//! Squad = 一组 agent + 一个 leader,leader 把任务派给成员。直接复用本
//! crate 的 `TaskManager` / `TeamFile` 存储,**不改 Task schema**:
//!
//! - Squad name = team name(复用 [`crate::team::validate_team_name`]);
//! - Squad leader = `TeamFile` 里 `agent_id == "team-lead@<name>"` 的成员;
//! - Squad task = 普通任务,`list_id = squad_name`,经 leader 认领 /
//!   `owner` 字段表达分配关系;actor 落 `Task.metadata.actor`
//!   (经 [`Actor::merge_into_metadata`],不覆盖其它键)。
//!
//! 桌面端注释自述「不改核心 crate,落 app-core」—— 正是该进 zhuye-task
//! 的编排层能力:`delegate_next` 复用 `claim_next_available` 的 per-list
//! 原子认领,MCP 插件做文件级影子写会破坏这一原子性。
//!
//! 顺手修掉桌面端的信息损失:`to_team_file` 的 `created_at` / `joined_at`
//! 此前一律写 `UNIX_EPOCH`,现用 `SquadSpec.created_at_ms` 还原。

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::actor::{Actor, actor_from_team_member};
use crate::error::TaskError;
use crate::manager::{TaskManager, TaskPatch};
use crate::model::{Task, TaskId, TeamFile, TeamMemberSpec};
use crate::team::{lead_agent_id_for, validate_team_name};

/// Squad 成员(语义层,对应 [`TeamMemberSpec`])。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SquadMember {
    /// 成员 actor。
    pub actor: Actor,
    /// 角色(kebab-case;`"team-lead"` / `"architect"` / `"builder"` 等)。
    pub role: String,
    /// 模型 spec(可选;None 用默认)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 系统 prompt。
    #[serde(default)]
    pub system_prompt: String,
    /// 允许的工具白名单(空 = text-only)。
    #[serde(default)]
    pub allowed_tools: Vec<String>,
}

/// Squad 定义。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SquadSpec {
    /// Squad 名(= team name,合法 `[a-z0-9_-]+`,1..=64)。
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 领导者 actor(`kind == Lead`,`actor_id == "team-lead@<name>"`)。
    pub leader_actor: Actor,
    /// 成员列表(不含 leader)。
    #[serde(default)]
    pub members: Vec<SquadMember>,
    /// 创建时间(UNIX epoch 毫秒;0 = 由 `create_squad` 填当前时间)。
    pub created_at_ms: u64,
}

/// Squad 任务分配记录(语义层)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SquadTaskAssignment {
    pub task_id: TaskId,
    pub squad_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<Actor>,
    pub assigned_at_ms: u64,
}

/// Squad 错误。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SquadError {
    /// 名字非法。
    Invalid { message: String },
    /// Squad 不存在。
    NotFound { name: String },
    /// 底层 TaskManager 错误。
    TaskManager { message: String },
}

impl std::fmt::Display for SquadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SquadError::Invalid { message } => write!(f, "squad invalid: {message}"),
            SquadError::NotFound { name } => write!(f, "squad not found: {name}"),
            SquadError::TaskManager { message } => write!(f, "squad task manager: {message}"),
        }
    }
}

impl std::error::Error for SquadError {}

fn tm_err(e: TaskError) -> SquadError {
    SquadError::TaskManager {
        message: e.to_string(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn ms_to_system_time(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

/// Squad 管理器:在 [`TaskManager`] 之上提供 leader 委派语义层。
pub struct SquadManager {
    task_manager: Arc<TaskManager>,
}

impl SquadManager {
    /// 用共享 `TaskManager` 构造。
    pub fn new(task_manager: Arc<TaskManager>) -> Self {
        Self { task_manager }
    }

    /// 底层 TaskManager 句柄(供调用方直接做 task CRUD)。
    pub fn task_manager(&self) -> &Arc<TaskManager> {
        &self.task_manager
    }

    /// 校验 squad 名(复用 team 名规则)。
    pub fn validate_name(name: &str) -> Result<(), SquadError> {
        validate_team_name(name).map_err(|e| SquadError::Invalid {
            message: e.to_string(),
        })
    }

    /// `SquadSpec` → `TeamFile`。时间戳按 `created_at_ms` 还原(不再写
    /// `UNIX_EPOCH` 丢信息)。
    pub fn to_team_file(spec: &SquadSpec) -> TeamFile {
        let created = ms_to_system_time(spec.created_at_ms);
        let lead_member = TeamMemberSpec {
            agent_id: lead_agent_id_for(&spec.name),
            name: spec
                .leader_actor
                .display_name
                .clone()
                .unwrap_or_else(|| "team-lead".into()),
            role: "team-lead".into(),
            model: None,
            system_prompt: String::new(),
            allowed_tools: vec![],
            color: None,
            joined_at: created,
            session_id: None,
            subscriptions: vec![],
        };
        let mut members = vec![lead_member];
        for m in &spec.members {
            // actor_id 已是 `<role>@<team>` 则直接复用,否则按 role 构造。
            let agent_id = if m.actor.actor_id.contains('@') {
                m.actor.actor_id.clone()
            } else {
                format!("{}@{}", m.role, spec.name)
            };
            members.push(TeamMemberSpec {
                agent_id,
                name: m
                    .actor
                    .display_name
                    .clone()
                    .unwrap_or_else(|| m.role.clone()),
                role: m.role.clone(),
                model: m.model.clone(),
                system_prompt: m.system_prompt.clone(),
                allowed_tools: m.allowed_tools.clone(),
                color: None,
                joined_at: created,
                session_id: None,
                subscriptions: vec![],
            });
        }
        TeamFile {
            name: spec.name.clone(),
            description: spec.description.clone(),
            lead_agent_id: lead_agent_id_for(&spec.name),
            lead_session_id: None,
            members,
            created_at: created,
        }
    }

    /// `TeamFile` → `SquadSpec`(leader 从 `lead_agent_id` 匹配的成员取名)。
    pub fn from_team_file(team: &TeamFile) -> SquadSpec {
        let mut leader_actor = Actor::agent(&team.name, "team-lead");
        let mut members: Vec<SquadMember> = Vec::new();
        for m in &team.members {
            if m.agent_id == team.lead_agent_id {
                if !m.name.is_empty() {
                    leader_actor.display_name = Some(m.name.clone());
                }
                continue;
            }
            members.push(SquadMember {
                actor: actor_from_team_member(m),
                role: m.role.clone(),
                model: m.model.clone(),
                system_prompt: m.system_prompt.clone(),
                allowed_tools: m.allowed_tools.clone(),
            });
        }
        SquadSpec {
            name: team.name.clone(),
            description: team.description.clone(),
            leader_actor,
            members,
            created_at_ms: team
                .created_at
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }
    }

    /// 创建 / 覆盖一个 squad(upsert team)。leader actor 强制对齐为
    /// `team-lead@<name>`;`created_at_ms == 0` 填当前时间。
    pub async fn create_squad(&self, mut spec: SquadSpec) -> Result<(), SquadError> {
        Self::validate_name(&spec.name)?;
        let display = spec.leader_actor.display_name.take();
        spec.leader_actor = Actor::agent(&spec.name, "team-lead");
        if let Some(d) = display {
            spec.leader_actor.display_name = Some(d);
        }
        if spec.created_at_ms == 0 {
            spec.created_at_ms = now_ms();
        }
        let team = Self::to_team_file(&spec);
        self.task_manager.upsert_team(team).await.map_err(tm_err)
    }

    /// 列出所有 squad。
    pub async fn list_squads(&self) -> Result<Vec<SquadSpec>, SquadError> {
        let teams = self.task_manager.list_teams().await.map_err(tm_err)?;
        Ok(teams.iter().map(Self::from_team_file).collect())
    }

    /// 读单个 squad(不存在 → `NotFound`)。
    pub async fn get_squad(&self, name: &str) -> Result<SquadSpec, SquadError> {
        let team = self
            .task_manager
            .get_team(name)
            .await
            .map_err(|e| match e {
                TaskError::TeamNotFound(_) => SquadError::NotFound {
                    name: name.to_string(),
                },
                other => tm_err(other),
            })?;
        Ok(Self::from_team_file(&team))
    }

    /// 删除 squad(team 文件物理删除;task 不级联)。
    pub async fn delete_squad(&self, name: &str) -> Result<(), SquadError> {
        self.task_manager.delete_team(name).await.map_err(tm_err)
    }

    /// leader 原子认领下一个 Pending + unblocked 任务(复用
    /// `claim_next_available` 的 per-list 串行化)。`None` = 无可用任务。
    pub async fn delegate_next(
        &self,
        squad_name: &str,
        leader_actor_id: &str,
    ) -> Result<Option<Task>, SquadError> {
        let list = squad_name.to_string();
        self.task_manager
            .claim_next_available(&list, leader_actor_id)
            .await
            .map_err(tm_err)
    }

    /// 把任务分配给具体成员(写 `owner` + `metadata.actor`);`None` 清空。
    ///
    /// **保留既有 metadata**:read-modify-write,只合并 / 删除 `actor`
    /// 子键(经 [`Actor::merge_into_metadata`]),不会用空对象覆盖其它
    /// 工具 / 集成写入的字段。
    pub async fn assign_task(
        &self,
        squad_name: &str,
        task_id: TaskId,
        assignee_actor_id: Option<String>,
    ) -> Result<Task, SquadError> {
        let actor = assignee_actor_id.as_deref().map(Actor::from_agent_id);
        let list = squad_name.to_string();
        let current = self
            .task_manager
            .get_task(&list, task_id, false)
            .await
            .map_err(tm_err)?;
        let merged = Actor::merge_into_metadata(actor.as_ref(), &current.metadata);
        // owner 三态:Some(Some(v)) 设值 / Some(None) 清空。
        let patch = TaskPatch {
            owner: Some(assignee_actor_id),
            metadata: Some(merged),
            ..Default::default()
        };
        let outcome = self
            .task_manager
            .update_task(&list, task_id, patch)
            .await
            .map_err(tm_err)?;
        Ok(outcome.task)
    }
}

#[cfg(test)]
mod tests;
