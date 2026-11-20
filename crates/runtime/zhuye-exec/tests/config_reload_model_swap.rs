//! 端到端热重载切 model:从 `ReflectConfig` 变更 → `handle_reload` →
//! `AgentConfig.model` 实际更新。
//!
//! 这条路径覆盖 v0.2.2 P0(roadmap `热重载切 model`)的核心契约。
//! 不发真 HTTP 请求 —— 只用 `handle_reload` 公开入口直接验证 RwLock 写入。

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use zhuye_config::{ActiveSection, ModelGroupSection, ReflectConfig};
use zhuye_core::AgentConfig;
use zhuye_exec::handle_reload;
use zhuye_llm::ModelRegistry;
use zhuye_protocol::{Event, EventMsg};
use zhuye_tools::ToolRegistry;

/// 模型组的 `model` 字段从 `claude-3-5-sonnet-latest` 切到
/// `claude-3-haiku-20240307` 时,`AgentConfig.model` 必须随之更新。
#[tokio::test]
async fn model_field_change_propagates_to_agent_config() {
    // env 隔离:handle_reload 经 `resolve_model_spec` 读 `ZHUYE_MODEL`,且
    // mock 值优先级高于 TOML(v1.3 SDK 设计)。runner 门禁恒设
    // `ZHUYE_MODEL=mock`,不隔离则 old/new spec 恒相等,reload 永不换模
    // 型。保存 → 清除 → 恢复。
    let prior_model = std::env::var("ZHUYE_MODEL").ok();
    // SAFETY: 单线程 tokio 测试;断言 env-无关路径。
    unsafe { std::env::remove_var("ZHUYE_MODEL") };
    let registry = Arc::new(ModelRegistry::new());
    let (tx, mut rx) = mpsc::channel::<Event>(4);
    let old_cfg = ReflectConfig {
        active: ActiveSection {
            group: Some("anthropic".into()),
            ..Default::default()
        },
        model_group: vec![ModelGroupSection {
            id: "anthropic".into(),
            endpoint: "anthropic".into(),
            api_key: Some("sk-a".into()),
            model: Some("claude-3-5-sonnet-latest".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut new_cfg = old_cfg.clone();
    new_cfg.model_group[0].model = Some("claude-3-haiku-20240307".into());
    let path = PathBuf::from("/tmp/test.toml");
    let agent_cfg = AgentConfig::new("anthropic/claude-3-5-sonnet-latest", "/tmp");

    handle_reload(
        &old_cfg,
        &new_cfg,
        &registry,
        &agent_cfg,
        None,
        None,
        &path,
        &tx,
        None,
        &ToolRegistry::default(),
        None,
        None,
    )
    .await
    .expect("handle_reload ok");

    assert_eq!(
        agent_cfg.current_model(),
        "anthropic/claude-3-haiku-20240307",
        "AgentConfig.model must zhuye the TOML change"
    );
    if let Some(m) = prior_model {
        // SAFETY: 恢复进程 env。
        unsafe { std::env::set_var("ZHUYE_MODEL", m) };
    }

    // 显式 drop tx 让 channel 关闭,rx.recv() 在排空后返回 None 跳出循环。
    drop(tx);
    let mut saw_reloaded = false;
    let mut saw_session = false;
    while let Some(ev) = rx.recv().await {
        match ev.msg {
            EventMsg::SessionConfigured(sc) => {
                assert_eq!(sc.model, "anthropic/claude-3-haiku-20240307");
                saw_session = true;
            }
            EventMsg::ConfigReloaded(cr) => {
                assert!(cr.sections_changed.contains(&"model_group".to_string()));
                saw_reloaded = true;
            }
            _ => {}
        }
    }
    assert!(saw_session, "SessionConfigured must be emitted");
    assert!(saw_reloaded, "ConfigReloaded must be emitted");
}

/// 旧/新 cfg 完全相同 → 不动 `AgentConfig.model`、不发 SessionConfigured、
/// `sections_changed` 为空。
#[tokio::test]
async fn no_op_reload_does_not_touch_model() {
    let registry = Arc::new(ModelRegistry::new());
    let (tx, mut rx) = mpsc::channel::<Event>(2);
    let cfg = ReflectConfig {
        active: ActiveSection {
            group: Some("anthropic".into()),
            ..Default::default()
        },
        model_group: vec![ModelGroupSection {
            id: "anthropic".into(),
            endpoint: "anthropic".into(),
            api_key: Some("sk-a".into()),
            model: Some("claude-3-5-sonnet-latest".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let path = PathBuf::from("/tmp/test.toml");
    let agent_cfg = AgentConfig::new("anthropic/claude-3-5-sonnet-latest", "/tmp");

    handle_reload(
        &cfg,
        &cfg,
        &registry,
        &agent_cfg,
        None,
        None,
        &path,
        &tx,
        None,
        &ToolRegistry::default(),
        None,
        None,
    )
    .await
    .expect("handle_reload ok");

    assert_eq!(
        agent_cfg.current_model(),
        "anthropic/claude-3-5-sonnet-latest",
        "identical configs must not change model"
    );
    drop(tx);
    let mut saw_reloaded = false;
    while let Some(ev) = rx.recv().await {
        match ev.msg {
            EventMsg::SessionConfigured(_) => {
                panic!("SessionConfigured must not fire on no-op reload");
            }
            EventMsg::ConfigReloaded(cr) => {
                assert!(
                    cr.sections_changed.is_empty(),
                    "identical cfg must produce empty diff, got {:?}",
                    cr.sections_changed
                );
                saw_reloaded = true;
            }
            _ => {}
        }
    }
    assert!(saw_reloaded, "ConfigReloaded must still emit on no-op");
}
