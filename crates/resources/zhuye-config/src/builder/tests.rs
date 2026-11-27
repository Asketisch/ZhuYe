//! `builder` 模块单测。
//!
//! ⚠️ env 隔离口径:runner 门禁(pr 套件)会给本进程设 `ZHUYE_MODEL=mock`,
//! 直接调读 env 的公开入口(`active_group` / `resolve_model` /
//! `resolved_model_spec` / `apply_to_registry`)的断言在该环境下**必假
//! 失败**(mock 优先级高于 TOML 配置,属 v1.3 SDK 的设计行为)。本模块
//! 统一走 `_with_env(.., None, None)` 注入版 —— 这些测试验证 TOML 配置
//! 行为;env 行为由显式设置变量的测试(`resolve_model_env_*`)另行覆盖。

use super::*;

use std::time::Duration;

use crate::schema::{ActiveSection, CredentialConfig, McpTransport, ModelGroupSection};

/// 造一个 openai / anthropic 端点的模型组。
fn group(id: &str, endpoint: &str, base_url: &str) -> ModelGroupSection {
    ModelGroupSection {
        id: id.into(),
        endpoint: endpoint.into(),
        base_url: Some(base_url.into()),
        ..Default::default()
    }
}

fn cfg_with_anthropic(key: &str) -> ZhuyeConfig {
    ZhuyeConfig {
        model_group: vec![ModelGroupSection {
            id: "anthropic".into(),
            endpoint: "anthropic".into(),
            api_key: Some(key.into()),
            base_url: Some("https://example.test".into()),
            model: Some("claude-test".into()),
            timeout_secs: Some(30),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn cfg_with_openai(key: &str) -> ZhuyeConfig {
    ZhuyeConfig {
        model_group: vec![ModelGroupSection {
            id: "openai".into(),
            endpoint: "openai".into(),
            api_key: Some(key.into()),
            model: Some("gpt-test".into()),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn cfg_with_ollama(model: Option<&str>) -> ZhuyeConfig {
    ZhuyeConfig {
        model_group: vec![ModelGroupSection {
            id: "ollama".into(),
            endpoint: "ollama".into(),
            model: model.map(str::to_string),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn empty_config_yields_empty_registry() {
    let r = ZhuyeConfig::default()
        .to_registry_with_env(None, None)
        .unwrap();
    assert!(r.list().is_empty());
}

#[test]
fn registry_registers_anthropic_and_openai() {
    let cfg = ZhuyeConfig {
        model_group: vec![
            ModelGroupSection {
                id: "anthropic".into(),
                endpoint: "anthropic".into(),
                api_key: Some("sk-a".into()),
                ..Default::default()
            },
            ModelGroupSection {
                id: "openai".into(),
                endpoint: "openai".into(),
                api_key: Some("sk-o".into()),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let r = cfg.to_registry_with_env(None, None).unwrap();
    assert_eq!(
        r.list(),
        vec!["anthropic".to_string(), "openai".to_string()]
    );
    assert!(r.get("anthropic").is_some());
    assert!(r.get("openai").is_some());
}

#[test]
fn apply_to_registry_is_additive() {
    let r = ModelRegistry::new();
    cfg_with_anthropic("sk-a")
        .apply_to_registry_with_env(&r, None, None)
        .unwrap();
    cfg_with_openai("sk-o")
        .apply_to_registry_with_env(&r, None, None)
        .unwrap();
    assert_eq!(r.list().len(), 2);
}

/// 回归(v1.10 功能3):`apply_to_registry` 灌入 zhuye-llm 全局窗口覆盖表的
/// 必须是**合并表**(`effective_context_window_overrides`)——
/// `[model].context_window`(作用于活跃模型)也要进。此前只灌
/// `[context_windows]` 段,单模型用户最常见的 `[model] context_window = N`
/// 写法进不了 llm 侧,`output_budget` 读到的口径与引擎 `SessionConfigured`
/// 不一致。
///
/// 直接断言纯函数(`apply_to_registry` 传给 `set_context_window_overrides`
/// 的就是它的返回值),不读进程级全局表 —— 并行用例的
/// `apply_to_registry` 会随时清空全局表,读全局的断言在压测下必闪。
#[test]
fn apply_to_registry_feeds_merged_context_window_overrides() {
    let cfg = ZhuyeConfig {
        model_group: vec![ModelGroupSection {
            id: "anthropic".into(),
            endpoint: "anthropic".into(),
            api_key: Some("sk-a".into()),
            base_url: Some("https://example.test".into()),
            model: Some("zz-merge-model".into()),
            ..Default::default()
        }],
        active: ActiveSection {
            group: Some("anthropic".into()),
            ..Default::default()
        },
        model: Some(crate::schema::ModelSection {
            context_window: Some(262_144),
            ..Default::default()
        }),
        ..Default::default()
    };
    // [model].context_window 以(归一化后的)活跃模型为键进合并表;
    // 活跃模型带组前缀 / 大小写差异时同样归一到同一键。
    let merged = cfg.effective_context_window_overrides("anthropic/ZZ-Merge-Model");
    assert_eq!(merged.get("zz-merge-model"), Some(&262_144));
    // [context_windows] per-model 条目照常并入。
    let cfg2 = ZhuyeConfig {
        context_windows: Some(crate::schema::ContextWindowsSection {
            entries: [("zz-merge-other".to_string(), 32_768)].into(),
        }),
        ..cfg
    };
    let merged2 = cfg2.effective_context_window_overrides("anthropic/zz-merge-model");
    assert_eq!(merged2.get("zz-merge-other"), Some(&32_768));
    assert_eq!(merged2.get("zz-merge-model"), Some(&262_144));
}

// ── Ollama 端点 ─────────────────────────────────────────────────

#[test]
fn registry_registers_ollama_group() {
    let cfg = cfg_with_ollama(Some("llama3.2"));
    let r = cfg.to_registry_with_env(None, None).unwrap();
    assert!(r.get("ollama").is_some(), "ollama 组应被注册");
}

#[test]
fn active_group_canonicalizes_ollama_aliases() {
    // 显式 `group = "ollama"` → 选 ollama。
    let cfg = ZhuyeConfig {
        active: ActiveSection {
            group: Some("ollama".into()),
            ..Default::default()
        },
        model_group: vec![ModelGroupSection {
            id: "ollama".into(),
            endpoint: "ollama".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(
        cfg.active_group_inner(None, None).as_deref(),
        Some("ollama")
    );

    // alias `"local"` 也被 canonicalize。
    let cfg = ZhuyeConfig {
        active: ActiveSection {
            group: Some("Local".into()),
            ..Default::default()
        },
        model_group: vec![ModelGroupSection {
            id: "ollama".into(),
            endpoint: "ollama".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(
        cfg.active_group_inner(None, None).as_deref(),
        Some("ollama")
    );
}

#[test]
fn active_group_falls_back_to_ollama_when_group_present() {
    // 没显式 `active.group` → 但有 ollama 组 → fall-back 选 ollama。
    let cfg = cfg_with_ollama(Some("qwen2.5:7b"));
    assert_eq!(
        cfg.active_group_inner(None, None).as_deref(),
        Some("ollama")
    );
}

#[test]
fn resolve_model_ollama_returns_group_override() {
    let cfg = cfg_with_ollama(Some("qwen2.5:7b"));
    assert_eq!(
        cfg.resolve_model_inner("ollama", None, None),
        Some("qwen2.5:7b".to_string())
    );
}

/// 模型名剥前缀:**只剥已知组 id** + `mock`;厂商式命名原样保留。
#[test]
fn strip_group_prefix_strips_only_known_group_ids() {
    let cfg = ZhuyeConfig {
        model_group: vec![
            group("openai", "openai", "https://a.example.test"),
            group("my-glm", "openai", "https://b.example.test"),
        ],
        ..Default::default()
    };
    assert_eq!(cfg.strip_group_prefix("openai/m1"), "m1");
    assert_eq!(cfg.strip_group_prefix("my-glm/GLM-5"), "GLM-5");
    assert_eq!(cfg.strip_group_prefix("mock/mock-1"), "mock-1");
    // 未知前缀:整串保留(OpenRouter 式 / 本地模型名)。
    assert_eq!(
        cfg.strip_group_prefix("cohere/north-mini-code:free"),
        "cohere/north-mini-code:free"
    );
    assert_eq!(
        cfg.strip_group_prefix("spark-2/qwen3.8-27b-nvfp4"),
        "spark-2/qwen3.8-27b-nvfp4",
        "真实模型名的前缀不是组 id 时必须原样保留"
    );
    // 无前缀:原样返回。
    assert_eq!(cfg.strip_group_prefix("gpt-4o"), "gpt-4o");
    // 前缀后为空:视为异常形态,原样返回。
    assert_eq!(cfg.strip_group_prefix("openai/"), "openai/");
}

/// env 注入组合回归:`ZHUYE_MODEL=openai/m1` 的值经
/// `resolve_model_inner` 解析后,请求体拿到的是 `m1` 而非整串 spec。
#[test]
fn resolve_model_env_with_group_prefix_is_stripped() {
    let cfg = cfg_with_openai("sk-o");
    assert_eq!(
        cfg.resolve_model_inner("openai", None, Some("openai/m1")),
        Some("m1".to_string()),
        "已知组前缀必须剥掉"
    );
    // 未知前缀(OpenRouter 式)原样透传。
    assert_eq!(
        cfg.resolve_model_inner("openai", None, Some("cohere/north-mini-code:free")),
        Some("cohere/north-mini-code:free".to_string())
    );
    // mock 的 spec 形态走专属分支(既有语义不变)。
    assert_eq!(
        cfg.resolve_model_inner("mock", None, Some("mock/mock-1")),
        Some("mock-1".to_string())
    );
}

/// v1.5 诚实化:未显式配置任何 model → `None`(不再编造内置默认)。
#[test]
fn resolve_model_returns_none_when_nothing_configured() {
    let cfg = ZhuyeConfig::default();
    assert_eq!(cfg.resolve_model_inner("ollama", None, None), None);
    assert_eq!(cfg.resolve_model_inner("anthropic", None, None), None);
    assert_eq!(cfg.resolve_model_inner("openai", None, None), None);
    assert_eq!(cfg.resolve_model_inner("gemini", None, None), None);
}

#[test]
fn resolved_model_spec_for_ollama() {
    let cfg = ZhuyeConfig {
        active: ActiveSection {
            group: Some("ollama".into()),
            ..Default::default()
        },
        model_group: vec![ModelGroupSection {
            id: "ollama".into(),
            endpoint: "ollama".into(),
            model: Some("qwen2.5:7b".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(
        cfg.resolved_model_spec_with_env(None, None),
        Some("ollama/qwen2.5:7b".to_string())
    );
}

// ── 多凭据轮换:[[<组>.credentials]] 数组 ────────────────────────

/// `[[<组>.credentials]]` 数组非空 → builder 展开为 N entry pool。
#[test]
fn credentials_array_builds_multi_entry_pool() {
    let toml = r#"
        [[model_group]]
        id = "anthropic"
        endpoint = "anthropic"

        [[model_group.credentials]]
        label = "work"
        api_key = "sk-work"
        weight = 2

        [[model_group.credentials]]
        label = "personal"
        api_key = "sk-personal"
    "#;
    let cfg: ZhuyeConfig = toml::from_str(toml).unwrap();
    let r = cfg.to_registry_with_env(None, None).unwrap();
    let healthy = r.healthy_clients("anthropic/claude-3-5-sonnet-latest");
    assert_eq!(healthy.len(), 2, "应有 work + personal 两个 entry");
    // healthy_clients 顺序 = pool.entries 插入顺序
    assert_eq!(healthy[0].label, "work");
    assert_eq!(healthy[1].label, "personal");
    // work weight=2, personal weight=1 → 3 次循环里 work 应得 2 次
    let mut work = 0;
    let mut personal = 0;
    for _ in 0..3 {
        match r.next_for("anthropic/x", &[]).unwrap().label.as_str() {
            "work" => work += 1,
            "personal" => personal += 1,
            _ => panic!("unexpected label"),
        }
    }
    assert_eq!(work, 2);
    assert_eq!(personal, 1);
}

/// 组级 `api_key` 单值形态注册为单 entry "default" 池。
#[test]
fn group_api_key_wraps_to_default_credential() {
    let cfg = ZhuyeConfig {
        model_group: vec![ModelGroupSection {
            id: "anthropic".into(),
            endpoint: "anthropic".into(),
            api_key: Some("sk-a".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let r = cfg.to_registry_with_env(None, None).unwrap();
    let nc = r.next_for("anthropic/x", &[]).unwrap();
    assert_eq!(nc.label, "default");
}

/// `credentials` 数组与组级 `api_key` 同时存在 → 数组优先。
#[test]
fn credentials_array_takes_precedence_over_group_api_key() {
    let cfg = ZhuyeConfig {
        model_group: vec![ModelGroupSection {
            id: "anthropic".into(),
            endpoint: "anthropic".into(),
            api_key: Some("sk-IGNORED".into()),
            credentials: vec![CredentialConfig {
                label: "only".into(),
                api_key: "sk-array".into(),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    let r = cfg.to_registry_with_env(None, None).unwrap();
    let healthy = r.healthy_clients("anthropic/x");
    assert_eq!(healthy.len(), 1);
    assert_eq!(healthy[0].label, "only");
}

#[test]
fn no_ollama_group_means_no_ollama_pool() {
    // 无 ollama 组 → 不注册。
    let cfg = ZhuyeConfig::default();
    let r = cfg.to_registry_with_env(None, None).unwrap();
    assert!(r.get("ollama").is_none());
    // active_group 依赖 env 兜底,只断言不 panic 即可。
    let _ = cfg.active_group_inner(None, None);
}

#[test]
fn canonical_group_round_trip_includes_ollama() {
    for (raw, want) in [
        ("ollama", Some("ollama")),
        ("Ollama", Some("ollama")),
        ("local", Some("ollama")),
        ("LOCAL", Some("ollama")),
        ("gemini", None),
    ] {
        assert_eq!(canonical_group(raw), want, "raw={raw}");
    }
}

/// 既没 key 也没 base_url 的组 → 不注册(用户没配,不造空池)。
#[test]
fn keyless_group_without_base_url_still_skipped() {
    let cfg = ZhuyeConfig {
        model_group: vec![ModelGroupSection {
            id: "openai".into(),
            endpoint: "openai".into(),
            api_key: Some(String::new()),
            base_url: None,
            ..Default::default()
        }],
        ..Default::default()
    };
    let r = cfg.to_registry_with_env(None, None).unwrap();
    assert!(r.get("openai").is_none());
}

/// **免 Key 端点也要注册** —— 显式 `base_url` + 空 `api_key`。
///
/// 免 Key 的 OpenAI 兼容端点(免费模型池)是真实接入形态。此前
/// 「key 空 → 不注册」把它挡在门外,只能用 `[[openai.credentials]]`
/// 数组形态绕道 —— 配置形态被迫分叉,GUI 也无法表达。
#[test]
fn keyless_group_with_base_url_registers_pool() {
    let cfg = ZhuyeConfig {
        model_group: vec![ModelGroupSection {
            id: "openai".into(),
            endpoint: "openai".into(),
            api_key: Some(String::new()),
            base_url: Some("https://api.kilo.ai/api/gateway".into()),
            model: Some("free".into()),
            timeout_secs: Some(300),
            ..Default::default()
        }],
        ..Default::default()
    };
    let r = cfg.to_registry_with_env(None, None).unwrap();
    assert!(
        r.get("openai").is_some(),
        "免 Key 端点(显式 base_url)必须注册 —— 否则 model=\"free\" 的清单发现报「未注册任何凭据」"
    );
}

/// 免 Key 端点也应能推出活跃组(与注册条件同口径)。
#[test]
fn keyless_group_with_base_url_is_active_group() {
    let cfg = ZhuyeConfig {
        model_group: vec![ModelGroupSection {
            id: "openai".into(),
            endpoint: "openai".into(),
            api_key: Some(String::new()),
            base_url: Some("https://api.kilo.ai/api/gateway".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(
        cfg.active_group_inner(None, None).as_deref(),
        Some("openai")
    );
}

/// `[[<组>.credentials]]` 的 `api_key` **可以整条省略** —— GUI 的 plan
/// 编辑器在 key 为空时会省略该行,此前写出的 TOML 直接反序列化失败
/// (整份配置保存被拒)。
#[test]
fn credential_block_without_api_key_deserializes() {
    let toml = r#"
        [[model_group]]
        id = "openai"
        endpoint = "openai"

        [[model_group.credentials]]
        label = "kilo-free"
        base_url = "https://api.kilo.ai/api/gateway"
        model = "free"
    "#;
    let cfg: ZhuyeConfig = toml::from_str(toml).expect("缺省 api_key 的凭据块应能反序列化");
    let cred = &cfg.model_group[0].credentials[0];
    assert_eq!(cred.label, "kilo-free");
    assert!(cred.api_key.is_empty(), "缺省 api_key = 空串(免 Key)");
}

#[test]
fn active_group_explicit_in_toml() {
    let cfg = ZhuyeConfig {
        active: ActiveSection {
            group: Some("openai".into()),
            ..Default::default()
        },
        model_group: vec![
            ModelGroupSection {
                id: "anthropic".into(),
                endpoint: "anthropic".into(),
                api_key: Some("sk-a".into()),
                ..Default::default()
            },
            ModelGroupSection {
                id: "openai".into(),
                endpoint: "openai".into(),
                api_key: Some("sk-o".into()),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    assert_eq!(
        cfg.active_group_inner(None, None).as_deref(),
        Some("openai")
    );
}

/// `[active].group` 指向**不存在的组** → 继续往下兜底(首个有凭据的组)。
#[test]
fn active_group_unknown_id_falls_through() {
    let cfg = ZhuyeConfig {
        active: ActiveSection {
            group: Some("gemini".into()),
            ..Default::default()
        },
        model_group: vec![ModelGroupSection {
            id: "openai".into(),
            endpoint: "openai".into(),
            api_key: Some("sk-o".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(
        cfg.active_group_inner(None, None).as_deref(),
        Some("openai")
    );
}

#[test]
fn active_group_falls_back_to_first_nonempty() {
    let cfg = cfg_with_anthropic("sk-a");
    assert_eq!(
        cfg.active_group_inner(None, None).as_deref(),
        Some("anthropic")
    );
}

#[test]
fn active_group_canonicalizes_aliases() {
    let cfg = ZhuyeConfig {
        active: ActiveSection {
            group: Some("Claude".into()),
            ..Default::default()
        },
        model_group: vec![ModelGroupSection {
            id: "anthropic".into(),
            endpoint: "anthropic".into(),
            api_key: Some("sk-a".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(
        cfg.active_group_inner(None, None).as_deref(),
        Some("anthropic")
    );

    let cfg = ZhuyeConfig {
        active: ActiveSection {
            group: Some("gpt".into()),
            ..Default::default()
        },
        model_group: vec![ModelGroupSection {
            id: "openai".into(),
            endpoint: "openai".into(),
            api_key: Some("sk-o".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(
        cfg.active_group_inner(None, None).as_deref(),
        Some("openai")
    );
}

#[test]
fn active_group_unknown_string_returns_none() {
    let cfg = ZhuyeConfig {
        active: ActiveSection {
            group: Some("gemini".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(cfg.active_group_inner(None, None), None);
}

#[test]
fn resolve_model_returns_group_override() {
    let cfg = cfg_with_anthropic("sk-a");
    assert_eq!(
        cfg.resolve_model_inner("anthropic", None, None),
        Some("claude-test".to_string())
    );
}

/// v1.5 诚实化:组级 / env / 钉住条目都没有 → `None`,不再回落内置默认。
#[test]
fn resolve_model_returns_none_when_group_missing() {
    let cfg = ZhuyeConfig::default();
    assert_eq!(cfg.resolve_model_inner("anthropic", None, None), None);
    assert_eq!(cfg.resolve_model_inner("openai", None, None), None);
    assert_eq!(cfg.resolve_model_inner("gemini", None, None), None);
}

#[test]
fn canonical_group_round_trip() {
    for (raw, want) in [
        ("anthropic", Some("anthropic")),
        ("Anthropic", Some("anthropic")),
        ("claude", Some("anthropic")),
        ("Claude", Some("anthropic")),
        ("openai", Some("openai")),
        ("GPT", Some("openai")),
        ("gemini", None),
        ("", None),
    ] {
        assert_eq!(canonical_group(raw), want, "raw={raw}");
    }
}

// ── resolved_model_spec ─────────────────────────────────────────────

/// 组级 `model` 覆盖默认,拼接为 `<组id>/<model>`。
#[test]
fn resolved_model_spec_uses_group_override() {
    let cfg = cfg_with_anthropic("sk-a");
    assert_eq!(
        cfg.resolved_model_spec_with_env(None, None),
        Some("anthropic/claude-test".to_string())
    );
}

/// `[active].group` 优先于「首个有凭据的组」的隐式回退。
#[test]
fn resolved_model_spec_prefers_active_group() {
    let cfg = ZhuyeConfig {
        active: ActiveSection {
            group: Some("openai".into()),
            ..Default::default()
        },
        model_group: vec![
            ModelGroupSection {
                id: "anthropic".into(),
                endpoint: "anthropic".into(),
                api_key: Some("sk-a".into()),
                ..Default::default()
            },
            ModelGroupSection {
                id: "openai".into(),
                endpoint: "openai".into(),
                api_key: Some("sk-o".into()),
                model: Some("gpt-test".into()),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    assert_eq!(
        cfg.resolved_model_spec_with_env(None, None),
        Some("openai/gpt-test".to_string())
    );
}

/// 没有可用组(`api_key` 空 + env 未设)→ `None`。
#[test]
fn resolved_model_spec_returns_none_when_no_group() {
    let cfg = ZhuyeConfig::default();
    let result = cfg.resolved_model_spec_with_env(None, None);
    // env 兜底可能让结果非 None(宿主环境可能设了 ANTHROPIC_API_KEY 等);
    // 此处只验证类型,不把 env 状态写死。
    let _: Option<String> = result;
}

/// 组已配置但没有任何显式 model → `None`(诚实化:GUI/TUI 据此
/// 显示"未配置模型",而不是编造 `claude-3-5-sonnet-latest` 之类默认值)。
#[test]
fn resolved_model_spec_returns_none_when_no_model_configured() {
    let cfg = ZhuyeConfig {
        active: ActiveSection {
            group: Some("openai".into()),
            ..Default::default()
        },
        model_group: vec![ModelGroupSection {
            id: "openai".into(),
            endpoint: "openai".into(),
            api_key: Some("sk-o".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(cfg.resolved_model_spec_with_env(None, None), None);
}

// ── `[active].credential` 钉住 + 条目级 model ─────────────────────

/// `[active].credential` 命中条目时,该条目的 `model` 优先于组级覆盖。
#[test]
fn resolve_model_prefers_pinned_credential_model() {
    let toml = r#"
        [active]
        group = "anthropic"
        credential = "minimax"

        [[model_group]]
        id = "anthropic"
        endpoint = "anthropic"
        model = "group-model"

        [[model_group.credentials]]
        label = "minimax"
        api_key = "sk-m"

        [[model_group.credentials]]
        label = "other"
        api_key = "sk-o"
        model = "other-model"
    "#;
    let cfg: ZhuyeConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.active_credential().as_deref(), Some("minimax"));
    // 命中条目无 model → 回落组级;不因别的条目有 model 而误取。
    assert_eq!(
        cfg.resolve_model_inner("anthropic", None, None),
        Some("group-model".to_string())
    );

    // 被钉住的条目自带 model → 压过组级。
    let toml = toml.replace(
        "api_key = \"sk-m\"",
        "api_key = \"sk-m\"\n        model = \"minimax-model\"",
    );
    let cfg: ZhuyeConfig = toml::from_str(&toml).unwrap();
    assert_eq!(
        cfg.resolve_model_inner("anthropic", None, None),
        Some("minimax-model".to_string())
    );
    assert_eq!(
        cfg.resolved_model_spec_with_env(None, None),
        Some("anthropic/minimax-model".to_string())
    );
}

/// 钉住只对活跃组生效:解析别的组时仍走组级。
#[test]
fn resolve_model_pin_applies_only_to_active_group() {
    let toml = r#"
        [active]
        group = "anthropic"
        credential = "minimax"

        [[model_group]]
        id = "anthropic"
        endpoint = "anthropic"

        [[model_group.credentials]]
        label = "minimax"
        api_key = "sk-m"
        model = "minimax-model"

        [[model_group]]
        id = "openai"
        endpoint = "openai"
        model = "gpt-group"
    "#;
    let cfg: ZhuyeConfig = toml::from_str(toml).unwrap();
    assert_eq!(
        cfg.resolve_model_inner("openai", None, None),
        Some("gpt-group".to_string()),
        "openai 不是活跃组,不受 anthropic 的钉住影响"
    );
}

/// `active_credential` 的 trim + 空串过滤。
#[test]
fn active_credential_trims_and_filters_empty() {
    let toml = r#"
        [active]
        group = "anthropic"
        credential = "  MiniMax  "
    "#;
    let cfg: ZhuyeConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.active_credential().as_deref(), Some("MiniMax"));

    let toml = r#"
        [active]
        group = "anthropic"
        credential = "   "
    "#;
    let cfg: ZhuyeConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.active_credential(), None);
}

/// `apply_to_registry` 把 `[active].credential` 注册为 preferred label。
#[test]
fn apply_to_registry_sets_preferred_label() {
    let toml = r#"
        [active]
        group = "anthropic"
        credential = "minimax"

        [[model_group]]
        id = "anthropic"
        endpoint = "anthropic"

        [[model_group.credentials]]
        label = "minimax"
        api_key = "sk-m"

        [[model_group.credentials]]
        label = "backup"
        api_key = "sk-b"
    "#;
    let cfg: ZhuyeConfig = toml::from_str(toml).unwrap();
    let r = cfg.to_registry_with_env(None, None).unwrap();
    assert_eq!(r.preferred_label("anthropic").as_deref(), Some("minimax"));
    assert_eq!(r.preferred_label("openai"), None);

    // 未钉住 → preferred 不设。
    let toml = toml.replace("credential = \"minimax\"", "");
    let cfg: ZhuyeConfig = toml::from_str(&toml).unwrap();
    let r = cfg.to_registry_with_env(None, None).unwrap();
    assert_eq!(r.preferred_label("anthropic"), None);
}

// ── MCP (v0.3) ────────────────────────────────────────────────────────

fn cfg_with_mcp_stdio(name: &str, cmd: &str) -> ZhuyeConfig {
    let toml = format!(
        r#"
        [mcp_servers.{name}]
        type = "stdio"
        command = "{cmd}"
        args = ["--port", "9000"]
        timeout_ms = 5000
        "#
    );
    toml::from_str(&toml).unwrap()
}

fn cfg_with_mcp_http(name: &str, url: &str) -> ZhuyeConfig {
    let toml = format!(
        r#"
        [mcp_servers.{name}]
        type = "streamable-http"
        url = "{url}"
        headers = {{ Authorization = "Bearer t" }}
        "#
    );
    toml::from_str(&toml).unwrap()
}

#[test]
fn mcp_server_configs_validates_stdio() {
    let cfg = cfg_with_mcp_stdio("fs", "npx");
    let cfgs = cfg.mcp_server_configs().unwrap();
    assert_eq!(cfgs.len(), 1);
    assert_eq!(cfgs[0].name, "fs");
    assert_eq!(cfgs[0].transport, McpTransport::Stdio);
    assert_eq!(cfgs[0].command.as_deref(), Some("npx"));
    assert_eq!(cfgs[0].args, vec!["--port", "9000"]);
    assert_eq!(cfgs[0].timeout, Duration::from_millis(5_000));
}

#[test]
fn mcp_server_configs_validates_http() {
    let cfg = cfg_with_mcp_http("github", "https://mcp.example.com/github");
    let cfgs = cfg.mcp_server_configs().unwrap();
    assert_eq!(cfgs.len(), 1);
    assert_eq!(cfgs[0].transport, McpTransport::Http);
    assert_eq!(
        cfgs[0].url.as_deref(),
        Some("https://mcp.example.com/github")
    );
    assert_eq!(
        cfgs[0].headers.get("Authorization").map(String::as_str),
        Some("Bearer t")
    );
    // 未指定 timeout_ms → 30s 默认
    assert_eq!(
        cfgs[0].timeout,
        Duration::from_millis(DEFAULT_MCP_TIMEOUT_MS)
    );
}

#[test]
fn mcp_server_configs_errors_on_stdio_without_command() {
    let cfg: ZhuyeConfig = toml::from_str(
        r#"
        [mcp_servers.bad]
        type = "stdio"
        "#,
    )
    .unwrap();
    let err = cfg.mcp_server_configs().unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("stdio requires 'command'"), "got: {msg}");
    assert!(msg.contains("[mcp_servers.bad]"), "got: {msg}");
}

#[test]
fn mcp_server_configs_errors_on_http_without_url() {
    let cfg: ZhuyeConfig = toml::from_str(
        r#"
        [mcp_servers.bad]
        type = "http"
        "#,
    )
    .unwrap();
    let err = cfg.mcp_server_configs().unwrap_err();
    assert!(err.to_string().contains("http requires 'url'"));
}

#[test]
fn mcp_server_configs_results_are_sorted_by_name() {
    let cfg: ZhuyeConfig = toml::from_str(
        r#"
        [mcp_servers.zeta]
        command = "z"
        [mcp_servers.alpha]
        command = "a"
        [mcp_servers.mid]
        command = "m"
        "#,
    )
    .unwrap();
    let cfgs = cfg.mcp_server_configs().unwrap();
    let names: Vec<&str> = cfgs.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "mid", "zeta"]);
}

// ── v1.3 SDK:mock 组 接线 ───────────────────────────────────────
// 走纯函数核心(`active_group_inner` / `resolve_model_inner`)断言,
// 不改写进程 env,避免与并行测试竞态。

/// mock 的三种 opt-in 路径与 model 前缀剥离。
#[test]
fn mock_group_opt_in_paths_and_model_resolution() {
    let cfg = ZhuyeConfig::default();
    // `ZHUYE_PROVIDER=mock` 显式 opt-in。
    assert_eq!(
        cfg.active_group_inner(Some("mock"), None).as_deref(),
        Some("mock")
    );
    assert_eq!(
        cfg.active_group_inner(Some("MOCK"), None).as_deref(),
        Some("mock")
    );
    // `ZHUYE_MODEL` 完整 spec / 裸 mock 均可 opt-in。
    assert_eq!(
        cfg.active_group_inner(None, Some("mock/mock-1")).as_deref(),
        Some("mock")
    );
    assert_eq!(
        cfg.active_group_inner(None, Some(" mock ")).as_deref(),
        Some("mock")
    );
    // 非 mock 的 ZHUYE_MODEL 不触发(回退后续优先级链)。
    assert_eq!(cfg.active_group_inner(None, Some("gpt-4o")), None);
    // 显式组优先于 model。
    assert_eq!(
        cfg.active_group_inner(Some("anthropic"), Some("mock/mock-1"))
            .as_deref(),
        Some("anthropic")
    );
    // model 解析:剥前缀 / 裸 mock 回退内置 mock model(mock 是显式
    // opt-in 的离线组,内置 model 名属于其契约)。
    assert_eq!(
        cfg.resolve_model_inner("mock", None, Some("mock/mock-1")),
        Some("mock-1".to_string())
    );
    assert_eq!(
        cfg.resolve_model_inner("mock", None, Some("mock")),
        Some("mock-1".to_string())
    );
    assert_eq!(
        cfg.resolve_model_inner("mock", None, None),
        Some("mock-1".to_string())
    );
}

/// 显式真实组时 mock 不参与活跃组选择。
#[test]
fn mock_not_active_for_real_group_config() {
    let cfg = cfg_with_anthropic("sk-a");
    assert_eq!(
        cfg.active_group_inner(Some("anthropic"), None).as_deref(),
        Some("anthropic")
    );
    assert_eq!(
        cfg.active_group_inner(None, None).as_deref(),
        Some("anthropic")
    );
}

// ── v1.5 review:`[routing] max_attempts` 配置透传 ────────────────

/// `[routing] max_attempts` 覆盖 `RoutingPolicy.max_attempts`(v1.10 缺省 10)。
#[test]
fn routing_max_attempts_from_toml() {
    // 显式配置 → 生效。
    let toml = r#"
        [routing]
        max_attempts = 4
    "#;
    let cfg: ZhuyeConfig = toml::from_str(toml).unwrap();
    assert_eq!(cfg.routing_policy().max_attempts, 4);

    // `[routing]` 段存在但未写 max_attempts → 缺省 10
    // (初始 1 次 + 最多 9 次自动重试)。
    let toml_default = r#"
        [routing]
        [routing.main]
        primary = "anthropic/claude-test"
    "#;
    let cfg_default: ZhuyeConfig = toml::from_str(toml_default).unwrap();
    assert_eq!(cfg_default.routing_policy().max_attempts, 10);

    // 整个 `[routing]` 段缺省 → 缺省 10。
    let cfg_none = cfg_with_anthropic("sk-a");
    assert_eq!(cfg_none.routing_policy().max_attempts, 10);
}

// ── 模型组([[model_group]]) ──────────────────────────────────────

/// openai 格式的组以组 id 为池名注册;`[active].group` 直接写组 id。
#[test]
fn model_group_registers_pool_under_group_id() {
    let cfg = ZhuyeConfig {
        active: ActiveSection {
            group: Some("my-glm".into()),
            ..Default::default()
        },
        model_group: vec![group(
            "my-glm",
            "openai",
            "https://open.bigmodel.cn/api/paas/v4",
        )],
        ..Default::default()
    };
    let r = cfg.to_registry_with_env(None, None).unwrap();
    assert!(r.get("my-glm").is_some(), "组 id 应注册为池名");
    assert_eq!(
        cfg.active_group_inner(None, None).as_deref(),
        Some("my-glm"),
        "[active].group = 组 id 应直接解析为该组"
    );
}

/// anthropic 格式的组同样按组 id 注册(走 AnthropicClient)。
#[test]
fn model_group_anthropic_endpoint_registers_pool() {
    let cfg = ZhuyeConfig {
        model_group: vec![group("magpie", "anthropic", "http://127.0.0.1:3425")],
        ..Default::default()
    };
    let r = cfg.to_registry_with_env(None, None).unwrap();
    assert!(r.get("magpie").is_some());
    // 无 [active].group 时首个有凭据的组作为隐式活跃组。
    assert_eq!(
        cfg.active_group_inner(None, None).as_deref(),
        Some("magpie")
    );
}

/// ollama 端点的组同样按组 id 注册(走 OllamaClient)—— 本地模型可用。
#[test]
fn model_group_ollama_endpoint_registers_pool() {
    let cfg = ZhuyeConfig {
        model_group: vec![group("local", "ollama", "http://127.0.0.1:11434")],
        ..Default::default()
    };
    let r = cfg.to_registry_with_env(None, None).unwrap();
    assert!(r.get("local").is_some());
    assert_eq!(r.get("local").unwrap().name(), "ollama");
}

/// 组级 model 写入 spec:`<组id>/<model>`。
#[test]
fn model_group_resolved_model_spec_uses_group_prefix() {
    let mut g = group("my-glm", "openai", "https://open.bigmodel.cn/api/paas/v4");
    g.model = Some("GLM-5.3-Flash".into());
    let cfg = ZhuyeConfig {
        model_group: vec![g],
        ..Default::default()
    };
    assert_eq!(
        cfg.resolved_model_spec_with_env(None, None),
        Some("my-glm/GLM-5.3-Flash".to_string())
    );
}

/// 免 Key 组(空 api_key + base_url)也要注册,语义与固定 id 组同口径。
#[test]
fn model_group_keyless_with_base_url_registers() {
    let mut g = group("free-pool", "openai", "https://free.example.test");
    g.api_key = Some(String::new());
    let cfg = ZhuyeConfig {
        model_group: vec![g],
        ..Default::default()
    };
    let r = cfg.to_registry_with_env(None, None).unwrap();
    assert!(r.get("free-pool").is_some());
}

/// 既没 key 也没 base_url 的组不注册(用户没配,不造空池)。
#[test]
fn model_group_without_credential_not_registered() {
    let cfg = ZhuyeConfig {
        model_group: vec![ModelGroupSection {
            id: "empty".into(),
            endpoint: "openai".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let r = cfg.to_registry_with_env(None, None).unwrap();
    assert!(r.get("empty").is_none());
    assert_eq!(cfg.active_group_inner(None, None), None);
}

/// 非法 id(含 '/' 或保留名 `mock`)与未知 endpoint 都跳过并 warn,
/// 不阻断其余组的注册。
#[test]
fn model_group_invalid_entries_skipped() {
    let cfg = ZhuyeConfig {
        model_group: vec![
            group("mock", "openai", "https://a.example.test"),
            group("a/b", "openai", "https://b.example.test"),
            group("weird", "grpc", "https://c.example.test"),
            group("good", "openai", "https://d.example.test"),
        ],
        ..Default::default()
    };
    let r = cfg.to_registry_with_env(None, None).unwrap();
    assert!(r.get("mock").is_none(), "与保留名冲突的组不注册");
    assert!(r.get("a/b").is_none(), "含 '/' 的组 id 不注册");
    assert!(r.get("weird").is_none(), "未知 endpoint 的组不注册");
    assert!(r.get("good").is_some(), "合法组不受影响");
}

/// 组间重复 id:先出现的注册,后出现的跳过(同名池覆盖保护)。
#[test]
fn model_group_duplicate_id_first_wins() {
    let cfg = ZhuyeConfig {
        model_group: vec![
            group("dup", "openai", "https://first.example.test"),
            group("dup", "anthropic", "https://second.example.test"),
        ],
        ..Default::default()
    };
    let r = cfg.to_registry_with_env(None, None).unwrap();
    let client = r.get("dup").expect("首个同名组应注册");
    assert_eq!(client.name(), "openai", "先出现者胜出");
}

/// 组内多凭据池 + 钉住:`[active].credential` 命中组内条目时,
/// 条目 model 优先于组级 model。
#[test]
fn model_group_pin_credential_and_entry_model() {
    let mut g = group("my-glm", "openai", "https://open.bigmodel.cn/api/paas/v4");
    g.model = Some("group-model".into());
    g.credentials = vec![CredentialConfig {
        label: "backup".into(),
        api_key: "sk-b".into(),
        model: Some("entry-model".into()),
        ..Default::default()
    }];
    let cfg = ZhuyeConfig {
        active: ActiveSection {
            group: Some("my-glm".into()),
            credential: Some("backup".into()),
            ..Default::default()
        },
        model_group: vec![g],
        ..Default::default()
    };
    assert_eq!(
        cfg.resolved_model_spec_with_env(None, None),
        Some("my-glm/entry-model".to_string())
    );
}

/// `ZHUYE_MODEL` 的前缀是已知组 id 时 → 选中该组。
#[test]
fn env_model_prefix_selects_known_group() {
    let cfg = ZhuyeConfig {
        model_group: vec![
            group("my-glm", "openai", "https://open.bigmodel.cn/api/paas/v4"),
            group("other", "openai", "https://other.example.test"),
        ],
        ..Default::default()
    };
    assert_eq!(
        cfg.active_group_inner(None, Some("my-glm/GLM-5"))
            .as_deref(),
        Some("my-glm")
    );
    // 前缀不是已知组 id → 不选中任何组(回退首个有凭据的组)。
    assert_eq!(
        cfg.active_group_inner(None, Some("spark-2/qwen3.8"))
            .as_deref(),
        Some("my-glm")
    );
}

/// `[active].group` 优先于 `ZHUYE_MODEL` 前缀。
#[test]
fn active_group_wins_over_env_model_prefix() {
    let cfg = ZhuyeConfig {
        active: ActiveSection {
            group: Some("other".into()),
            ..Default::default()
        },
        model_group: vec![
            group("my-glm", "openai", "https://open.bigmodel.cn/api/paas/v4"),
            group("other", "openai", "https://other.example.test"),
        ],
        ..Default::default()
    };
    assert_eq!(
        cfg.active_group_inner(None, Some("my-glm/GLM-5"))
            .as_deref(),
        Some("other")
    );
}
