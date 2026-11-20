//! `zhuye doctor` —— 环境自检,best-effort 报告多项,任一失败不阻塞。

use zhuye_config::{default_config_path, load_default};
use zhuye_rollout::index::list_sessions;

const OK: &str = "✓";
const WARN: &str = "!";

/// v0.4+: `zhuye doctor` 子命令的实现。
///
/// 检查项(config / provider / rollout / MCP / LSP / plugins / rustc / sessions),
/// 整体 exit code 0(doctor 是诊断,不是校验)。
/// v1.11 F6:`dump_config = true` 时打印生效配置 JSON 快照并跳过诊断
/// (组合透明性,对齐 DSH `--dump-config`;报告的是「引擎实际会装配什么」)。
pub async fn run(check_network: bool, dump_config: bool) -> anyhow::Result<()> {
    let cfg = load_default();
    let config_path = default_config_path();
    if dump_config {
        println!(
            "{}",
            serde_json::to_string_pretty(&dump_effective_config(&cfg)).expect("dump serialize")
        );
        return Ok(());
    }

    println!("ZhuYe doctor");
    println!("==================");

    // ── 1. 配置(config)──
    match &config_path {
        Some(p) if p.exists() => {
            println!("{OK} config    {} (parsed ok)", p.display());
        }
        Some(p) => {
            println!(
                "{WARN} config    {} (not present; using defaults)",
                p.display()
            );
        }
        None => {
            println!("{WARN} config    HOME unset; cannot locate ~/.zhuye/config.toml");
        }
    }

    // ── 2. provider 可达性(provider reachability)──
    if check_network {
        if let Some(group) = cfg.active_group() {
            println!("[network] checking model group '{group}' (best-effort)...");
            probe_provider_network(&cfg, &group);
        } else {
            println!("{WARN} provider  no [active].group set; would skip network probe");
        }
    } else {
        println!("[skip]   provider  network probe skipped (pass --check-network to enable)");
    }

    // ── 3. 模型清单(model list)──
    // 清单实时取自 provider 接口 —— 免费模型池的成员集合随时会变,
    // 这一项告诉用户「现在到底有哪些模型可用」。
    if check_network {
        let registry = std::sync::Arc::new(zhuye_llm::ModelRegistry::new());
        if let Err(e) = cfg.apply_to_registry(&registry) {
            println!("{WARN} models    cannot build provider from config: {e}");
        } else {
            match crate::models::doctor_summary(&cfg, &registry).await {
                Some(summary) => println!("{OK} models    {summary}"),
                None => {
                    println!("[skip]   models    no [active].group set; skipped model list probe")
                }
            }
        }
    } else {
        println!("[skip]   models    model list probe skipped (pass --check-network to enable)");
    }

    // ── 4. rollout 目录(rollout dir)──
    let rollout_base = zhuye_rollout::path::default_base();
    if rollout_base.exists() {
        println!(
            "{OK} rollout   {} (writable: assumed)",
            rollout_base.display()
        );
    } else {
        println!(
            "{WARN} rollout   {} does not exist yet; will be created on first session",
            rollout_base.display()
        );
    }

    // ── 5. MCP 服务器(MCP servers)──
    check_mcp(&cfg);

    // ── 6. LSP 服务器(LSP servers)──
    check_lsp(&cfg);

    // ── 7. 插件(Plugins)──
    check_plugins(&cfg);

    // ── 8. rustc 与 workspace 版本(rustc + workspace version)──
    // v1.6:探测加超时 + stdin 置 null —— `rustc` 可能是 rustup shim,
    // 空 HOME 下会触发工具链网络下载而无限阻塞(e2e step 12 实测挂死
    // 25 分钟);诊断命令不允许卡死整个 doctor。
    let rustc = run_probe_with_timeout("rustc", &["--version"], std::time::Duration::from_secs(5))
        .unwrap_or_else(|| "rustc not on PATH (or probe timed out)".to_string());
    println!("[env]    rustc     {rustc}");
    println!("[env]    package   zhuye {}", env!("CARGO_PKG_VERSION"));

    // ── 9. session 计数(session count)──
    match list_sessions(&rollout_base) {
        Ok(sessions) => println!(
            "[data]   sessions  {} found under {}",
            sessions.len(),
            rollout_base.display()
        ),
        Err(e) => println!("{WARN} sessions  list_sessions failed: {e}"),
    }

    // ── 10. 集成桩(integration stubs,P2/P3)──
    check_integration_stubs(&cfg);

    println!();
    println!("Use `zhuye login` to configure credentials.");
    println!("Use `zhuye doctor --check-network` to also probe the provider.");
    Ok(())
}

/// 解析配置的 provider base_url,对主机做 TCP 连通性探测(3s 超时)。
///
/// 仅验证网络可达性,不发认证请求;对 401/403 之类无需区分(那是
/// `zhuye exec` 真实调用阶段的事)。Ollama 走明文 http,云厂商走 https。
fn probe_provider_network(cfg: &zhuye_config::ReflectConfig, provider: &str) {
    use std::net::TcpStream;
    use std::time::Duration;

    let Some(url) = active_base_url(cfg, provider) else {
        println!("{WARN} provider  no base_url resolved for '{provider}'");
        return;
    };
    let (host, port) = match parse_host_port(&url) {
        Ok(hp) => hp,
        Err(e) => {
            println!("{WARN} provider  malformed base_url '{url}': {e}");
            return;
        }
    };
    // DNS 解析 → 取首个地址 → 3s 超时 TCP 连接。失败不 panic,只告警。
    let addr_str = format!("{host}:{port}");
    match (host.as_str(), port).to_socket_addrs() {
        Ok(mut addrs) => match addrs.next() {
            Some(socket_addr) => {
                match TcpStream::connect_timeout(&socket_addr, Duration::from_secs(3)) {
                    Ok(_) => println!("{OK} provider  {url} reachable (tcp connect ok)"),
                    Err(e) => println!("{WARN} provider  {url} unreachable: {e}"),
                }
            }
            None => println!("{WARN} provider  {addr_str} resolved to no address"),
        },
        Err(e) => println!("{WARN} provider  resolve {addr_str} failed: {e}"),
    }
}

/// 从 config + 模型组推导 base_url,缺省走**该组端点**的厂商默认地址。
///
/// 组 id 不再决定厂商 —— 组自带 `endpoint`(openai / anthropic / ollama),
///默认地址按 endpoint 走;组未声明 endpoint 时按 id 兜底。
fn active_base_url(cfg: &zhuye_config::ReflectConfig, group_id: &str) -> Option<String> {
    let group = cfg.model_group.iter().find(|g| g.id.trim() == group_id);
    let group_url = group.and_then(|g| g.base_url.clone());
    let endpoint = group
        .map(|g| g.endpoint.trim().to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| group_id.to_ascii_lowercase());
    Some(group_url.unwrap_or_else(|| match endpoint.as_str() {
        "anthropic" => "https://api.anthropic.com".to_string(),
        "openai" => "https://api.openai.com".to_string(),
        "ollama" => "http://127.0.0.1:11434".to_string(),
        other => other.to_string(),
    }))
}

/// 把 `https://host[:port]/path` 拆成 (host, port),协议缺省 https:443 / http:80。
fn parse_host_port(url: &str) -> anyhow::Result<(String, u16)> {
    let (scheme_stripped, default_port) = if let Some(rest) = url.strip_prefix("https://") {
        (rest, 443u16)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (rest, 80u16)
    } else {
        (url, 443u16)
    };
    // 取 authority 段(第一个 '/' 之前),丢掉 path / query。
    let authority = scheme_stripped.split('/').next().unwrap_or(scheme_stripped);
    let authority = authority.split('?').next().unwrap_or(authority);
    let (host, port) = match authority.rsplit_once(':') {
        // 形如 host:port;IPv6 [::1]:port 的兜底由 to_socket_addrs 处理。
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(default_port)),
        None => (authority.to_string(), default_port),
    };
    if host.is_empty() {
        anyhow::bail!("empty host");
    }
    Ok((host, port))
}

// `to_socket_addrs` 需要 trait import(P2 `doctor-enhance` 网络探测)。
use std::net::ToSocketAddrs;

/// MCP 配置校验 + 摘要。
fn check_mcp(cfg: &zhuye_config::ReflectConfig) {
    if cfg.mcp_servers.servers.is_empty() {
        println!("[skip]   mcp       no [mcp_servers.*] configured");
        return;
    }
    match cfg.mcp_server_configs() {
        Ok(configs) => {
            let stdio = configs
                .iter()
                .filter(|c| c.transport == zhuye_config::McpTransport::Stdio)
                .count();
            let http = configs
                .iter()
                .filter(|c| c.transport == zhuye_config::McpTransport::Http)
                .count();
            let sse = configs
                .iter()
                .filter(|c| c.transport == zhuye_config::McpTransport::Sse)
                .count();
            println!(
                "{OK} mcp       {} server(s): stdio={stdio} http={http} sse={sse}",
                configs.len()
            );
            for c in &configs {
                let ty = match c.transport {
                    zhuye_config::McpTransport::Stdio => "stdio",
                    zhuye_config::McpTransport::Http => "http",
                    zhuye_config::McpTransport::Sse => "sse",
                };
                let endpoint = c
                    .command
                    .as_deref()
                    .or(c.url.as_deref())
                    .unwrap_or("<missing>");
                println!("         - {} ({ty}) {endpoint}", c.name);
            }
            println!("         (run `zhuye mcp test <name>` to validate runtime)");
        }
        Err(e) => {
            println!("{WARN} mcp       config invalid: {e}");
        }
    }
}

/// LSP 配置摘要。
fn check_lsp(cfg: &zhuye_config::ReflectConfig) {
    if cfg.lsp_servers.servers.is_empty() {
        println!("[skip]   lsp       no [lsp_servers.*] configured");
        return;
    }
    match cfg.lsp_server_configs() {
        Ok(configs) => {
            println!("{OK} lsp       {} server(s)", configs.len());
            for c in &configs {
                println!(
                    "         - {} cmd={} patterns={}",
                    c.name,
                    c.command,
                    c.patterns.len()
                );
            }
        }
        Err(e) => {
            println!("{WARN} lsp       config invalid: {e}");
        }
    }
}

/// P2/P3 集成 stub 摘要行。
fn check_integration_stubs(cfg: &zhuye_config::ReflectConfig) {
    use zhuye_config::analytics_status_line;
    use zhuye_integration::notifications::WebhookNotifier;
    use zhuye_stream::PostgresSessionConfig;
    use zhuye_stream::postgres_session;

    println!(
        "[stub]   notify    {}",
        WebhookNotifier::new(
            cfg.notifications
                .as_ref()
                .and_then(|n| n.webhook_url.clone())
        )
        .status_line()
    );
    let pg = cfg.postgres_session.as_ref().and_then(|p| {
        p.database_url.as_ref().map(|u| PostgresSessionConfig {
            database_url: u.clone(),
            table_prefix: p.table_prefix.clone(),
            pool_size: 4,
        })
    });
    println!(
        "[stub]   postgres  {}",
        postgres_session::status_line(pg.as_ref())
    );
    println!(
        "[v1.3]   analytics {}",
        analytics_status_line(cfg.analytics.as_ref())
    );
    println!(
        "[stub]   mcp-reg   {} catalog entries",
        zhuye_mcp::curated_catalog().len()
    );
    let audit = zhuye_integration::run_cargo_audit(None);
    println!("[stub]   security  {}", audit.summary);
}

/// 已安装 / 已启用 plugin 摘要。
fn check_plugins(cfg: &zhuye_config::ReflectConfig) {
    let enabled = cfg.plugins.enabled_plugins.len();
    let root = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".zhuye").join("plugins"));
    let installed = root
        .as_ref()
        .and_then(|p| zhuye_plugin::PluginManager::load(p).ok())
        .map_or(0, |m| m.list_installed().len());
    if installed == 0 && enabled == 0 {
        println!("[skip]   plugins   none installed");
        return;
    }
    println!("{OK} plugins   installed={installed} enabled={enabled}");
    if enabled > 0 {
        for id in &cfg.plugins.enabled_plugins {
            println!("         - {id} (enabled)");
        }
    }
}

/// 测试辅助:加载 config 并快照解析摘要。
/// 带超时运行外部探测命令,取 stdout 首行(stdin 置 null 防交互提示)。
///
/// `rustc` / `cargo` 等 PATH 上的二进制可能是 rustup shim:空 HOME 下
/// shim 会触发工具链网络下载而无限阻塞 —— 诊断探测必须有界。
/// 超时 / 非零退出 / spawn 失败一律返回 `None`(doctor 是尽力而为)。
fn run_probe_with_timeout(
    cmd: &str,
    args: &[&str],
    timeout: std::time::Duration,
) -> Option<String> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = std::process::Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out.trim().to_string());
            }
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

#[cfg(test)]
fn snapshot_summary(cfg: &zhuye_config::ReflectConfig) -> String {
    let mut out = String::new();
    let group_ids: Vec<&str> = cfg.model_group.iter().map(|g| g.id.as_str()).collect();
    out.push_str(&format!(
        "model_groups={} ({})\n",
        group_ids.len(),
        group_ids.join(",")
    ));
    out.push_str(&format!(
        "mcp_servers={} compact_trigger={:?}\n",
        cfg.mcp_servers.servers.len(),
        cfg.compact.trigger_tokens
    ));
    out
}

/// v1.11 F6:当前生效配置的 JSON 快照(组合透明性)。
///
/// 覆盖引擎装配的每个决策面:active provider / 解析后的 model spec /
/// 路由 slot / 凭据 label(只出 label,不出密钥值)/ hooks / MCP / LSP /
/// 插件 / 沙箱 / 预算 / 思考强度 / agent 定义目录 / 数据目录。
/// 机器可读(`zhuye doctor --dump-config | jq`),供排障与「到底是什么
/// 在生效」的一眼回答。
fn dump_effective_config(cfg: &zhuye_config::ReflectConfig) -> serde_json::Value {
    use serde_json::json;

    // provider / model:解析后的生效值(env 优先级已含)。
    let active_group = cfg.active_group();
    let model_spec = cfg.resolved_model_spec();

    // 凭据:只出 label 与「是否设了 key」,绝不出 key 本身。
    // 凭据条目摘要(label + 布尔,绝不出 key 值)。
    fn cred_labels(
        creds: Option<&Vec<zhuye_config::schema::CredentialConfig>>,
    ) -> Vec<serde_json::Value> {
        creds.map_or_default(|list| {
            list.iter()
                .map(|c| {
                    json!({
                        "label": c.label,
                        "has_api_key": !c.api_key.is_empty(),
                        "has_base_url": c.base_url.is_some(),
                        "model": c.model,
                    })
                })
                .collect()
        })
    }

    let routing = cfg.routing.as_ref();
    let routing_json = json!({
        "main": routing.map(|r| json!({
            "primary": r.main.primary,
            "fallbacks": r.main.fallbacks,
            "weights": r.main.weights,
            "complexity": r.main.complexity.as_ref().map(|c| json!({
                "low": c.low, "medium": c.medium, "high": c.high,
                "max": c.max, "ultra": c.ultra,
            })),
        })),
        "compact": routing.map(|r| json!({ "primary": r.compact.primary, "fallbacks": r.compact.fallbacks })),
        "subagent": routing.map(|r| json!({ "primary": r.subagent.primary, "fallbacks": r.subagent.fallbacks })),
    });

    let mcp: Vec<serde_json::Value> = match cfg.mcp_server_configs() {
        Ok(configs) => configs
            .iter()
            .map(|c| {
                json!({
                    "name": c.name,
                    "transport": c.transport,
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    };

    json!({
        "schema": "zhuye-dump-config/1",
        "version": env!("CARGO_PKG_VERSION"),
        "config_path": default_config_path().map(|p| p.display().to_string()),
        "data_dir": zhuye_config::data_path(&[]).map(|p| p.display().to_string()),
        "active": {
            "group": active_group,
            "credential": cfg.active.credential,
            "model_spec": model_spec,
            "max_iterations": cfg.active.max_iterations,
            "agents_md": cfg.active.agents_md,
            "bundled_skills": cfg.active.bundled_skills,
            "tools_allowlist": !cfg.active.tools.is_empty(),
            "disallowed_tools": cfg.active.disallowed_tools,
        },
        // 模型组是唯一的接入单元 —— 逐组列出(组 id → 摘要)。
        "model_groups": cfg.model_group.iter().map(|g| json!({
            "id": g.id,
            "endpoint": g.endpoint,
            "label": g.display_label(),
            "has_api_key": g.api_key.as_ref().is_some_and(|k| !k.is_empty()),
            "base_url": g.base_url,
            "model": g.model,
            "credentials": cred_labels(Some(&g.credentials)),
        })).collect::<Vec<_>>(),
        "routing": routing_json,
        "model": cfg.model.as_ref().map(|m| json!({
            "context_window": m.context_window,
            "reasoning_effort": m.reasoning_effort,
        })),
        "hooks": {
            "enabled": cfg.hooks.enabled,
            "disabled": cfg.hooks.disabled,
        },
        "mcp_servers": mcp,
        "lsp_servers": cfg.lsp_servers.servers.len(),
        "plugins": {
            "enabled_plugins": cfg.plugins.enabled_plugins,
            "marketplaces": cfg.plugins.marketplaces.keys().cloned().collect::<Vec<_>>(),
        },
        "sandbox": json!({
            "os_level": cfg.sandbox.os_level,
            "allow_network": cfg.sandbox.allow_network,
            "writable_dirs_count": cfg.sandbox.writable_dirs.len(),
        }),
        "token_budget": cfg.token_budget.as_ref().and_then(|t| t.session_total_tokens),
        "effort": cfg.resolve_reasoning_effort().map(|e| e.as_str()),
        "subagent_specs": cfg.subagents.len(),
        "coordinator_enabled": cfg.coordinator.as_ref().map(|c| c.enabled),
        "compact_trigger_tokens": cfg.compact.trigger_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use zhuye_config::{ModelGroupSection, ReflectConfig};

    #[tokio::test]
    async fn doctor_runs_without_panic_with_default() {
        let result = run(false, false).await;
        assert!(result.is_ok());
    }

    /// 快命令正常返回 stdout;卡死命令(超 sleep)超时后返回 None 并
    /// kill 子进程 —— 防 rustup shim 网络下载类挂死。
    #[test]
    fn probe_timeout_kills_stuck_command() {
        let out = run_probe_with_timeout(
            "/bin/echo",
            &["probe-ok"],
            std::time::Duration::from_secs(5),
        );
        assert_eq!(out.as_deref(), Some("probe-ok"));

        let started = std::time::Instant::now();
        let out =
            run_probe_with_timeout("/bin/sleep", &["30"], std::time::Duration::from_millis(300));
        assert!(out.is_none(), "卡死命令应超时返回 None");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "必须及时 kill,不能等子进程自然退出"
        );

        // 不存在的命令 → None(spawn 失败)。
        assert!(
            run_probe_with_timeout(
                "/nonexistent-zhuye-probe",
                &[],
                std::time::Duration::from_secs(5)
            )
            .is_none()
        );
    }

    #[test]
    fn snapshot_summary_lists_present_sections() {
        let cfg = ReflectConfig {
            model_group: vec![ModelGroupSection {
                id: "anthropic".into(),
                endpoint: "anthropic".into(),
                api_key: Some("sk-x".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let s = snapshot_summary(&cfg);
        assert!(s.contains("model_groups=1"));
        assert!(s.contains("anthropic"));
    }

    /// v1.11 F6:`--dump-config` 凭据摘要脱敏 —— 只出 label / has_api_key
    /// 布尔 / model,**任何 api_key 值都不得出现在整份 JSON 快照里**
    /// (段级与凭据池条目双路径都验)。
    #[test]
    fn dump_effective_config_脱敏凭据只出布尔() {
        use zhuye_config::schema::CredentialConfig;
        const SECRET: &str = "sk-ant-super-secret-doctor-test";
        let cfg = ReflectConfig {
            model_group: vec![ModelGroupSection {
                id: "anthropic".into(),
                endpoint: "anthropic".into(),
                api_key: Some("sk-ant-section-key".into()),
                credentials: vec![
                    CredentialConfig {
                        label: "pool-a".into(),
                        api_key: SECRET.into(),
                        ..Default::default()
                    },
                    CredentialConfig {
                        label: "pool-b".into(),
                        api_key: String::new(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let dump = dump_effective_config(&cfg);
        assert_eq!(dump["schema"], "zhuye-dump-config/1", "schema 标记固定");
        let anthropic = &dump["model_groups"][0];
        assert_eq!(anthropic["id"], "anthropic");
        assert_eq!(anthropic["has_api_key"], true, "组级 api_key 存在 → true");
        let creds = anthropic["credentials"].as_array().unwrap();
        assert_eq!(creds.len(), 2);
        assert_eq!(creds[0]["label"], "pool-a");
        assert_eq!(creds[0]["has_api_key"], true, "池条目有 key → true");
        assert_eq!(creds[1]["label"], "pool-b");
        assert_eq!(creds[1]["has_api_key"], false, "空 key 条目 → false");
        // 脱敏不变量:整份序列化输出不含任何密钥值,凭据条目无 api_key 字段。
        let raw = dump.to_string();
        assert!(!raw.contains(SECRET), "密钥值泄漏:{raw}");
        assert!(!raw.contains("sk-ant-section-key"), "段级密钥值泄漏:{raw}");
        for c in creds {
            assert!(
                c.get("api_key").is_none(),
                "凭据条目不得含 api_key 字段:{c}"
            );
        }
    }

    /// v1.11 F6 / 桌面#3:dump 的路由段反映 `[routing.main]` 的 primary /
    /// fallbacks / weights 与 complexity 档位映射(未配置档位为 null)。
    #[test]
    fn dump_effective_config_路由与复杂度映射() {
        use zhuye_config::schema::agent::{ComplexityRoutes, RoutingSection, SpecSlotConfig};
        let cfg = ReflectConfig {
            routing: Some(RoutingSection {
                main: SpecSlotConfig {
                    primary: Some("anthropic/claude-x".into()),
                    fallbacks: vec!["deepseek/r1".into()],
                    weights: vec![7, 3],
                    complexity: Some(ComplexityRoutes {
                        low: Some("openai/mini".into()),
                        high: Some("anthropic/claude-x".into()),
                        ..Default::default()
                    }),
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        let dump = dump_effective_config(&cfg);
        let main = &dump["routing"]["main"];
        assert_eq!(main["primary"], "anthropic/claude-x");
        assert_eq!(main["fallbacks"][0], "deepseek/r1");
        assert_eq!(main["weights"][0], 7);
        let complexity = &main["complexity"];
        assert_eq!(complexity["low"], "openai/mini");
        assert_eq!(complexity["high"], "anthropic/claude-x");
        assert_eq!(
            complexity["medium"],
            serde_json::Value::Null,
            "未配置档位应为 null(回退 primary)"
        );
        // 无 routing 段时整体仍可 dump 且不 panic。
        let empty = dump_effective_config(&ReflectConfig::default());
        assert_eq!(empty["routing"]["main"]["primary"], serde_json::Value::Null);
    }

    #[test]
    fn config_serializes_for_inspection() {
        let cfg = ReflectConfig::default();
        let s = toml::to_string_pretty(&cfg)
            .context("serialize default cfg")
            .unwrap();
        assert!(s.contains("[active]"));
        assert!(s.contains("[compact]"));
    }

    #[test]
    fn parse_host_port_https_default_443() {
        let (h, p) = parse_host_port("https://api.anthropic.com").unwrap();
        assert_eq!(h, "api.anthropic.com");
        assert_eq!(p, 443);
    }

    #[test]
    fn parse_host_port_http_custom_port() {
        let (h, p) = parse_host_port("http://127.0.0.1:11434/v1/").unwrap();
        assert_eq!(h, "127.0.0.1");
        assert_eq!(p, 11434);
    }

    #[test]
    fn parse_host_port_strips_path_and_query() {
        let (h, p) = parse_host_port("https://api.openai.com/chat?x=1").unwrap();
        assert_eq!(h, "api.openai.com");
        assert_eq!(p, 443);
    }

    #[test]
    fn parse_host_port_rejects_empty_host() {
        assert!(parse_host_port("https:///path").is_err());
    }

    #[test]
    fn active_base_url_uses_group_then_endpoint_defaults() {
        let cfg = ReflectConfig {
            model_group: vec![ModelGroupSection {
                id: "anthropic".into(),
                endpoint: "anthropic".into(),
                base_url: Some("https://my.proxy.example".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            active_base_url(&cfg, "anthropic").as_deref(),
            Some("https://my.proxy.example")
        );
        // 无该组时回退到厂商默认 endpoint。
        assert_eq!(
            active_base_url(&cfg, "openai").as_deref(),
            Some("https://api.openai.com")
        );
        assert_eq!(
            active_base_url(&cfg, "ollama").as_deref(),
            Some("http://127.0.0.1:11434")
        );
    }
}
