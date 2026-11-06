//! `zhuye-import` 独立二进制 —— 双模式:
//!
//! 1. **直跑模式**(默认):批量导入外部 Harness 会话到本地存储,
//!    供终端脚本 / 桌面端直接调用,不经 LLM:
//!    ```text
//!    zhuye-import <path...> --from auto [--all] [--dry-run] [--force] …
//!    ```
//! 2. **MCP 模式**(`zhuye-import mcp`):以 stdio MCP server 常驻,
//!    供 harness-import 插件在 CLI / Desktop 对话内经 LLM 调用。
//!
//! stdout 只承载结果(JSON / 导入报告 / JSON-RPC),诊断一律 stderr。

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use zhuye_import::model::{HarnessKind, ImportOptions, ImportSource};
use zhuye_import::{discover, import_all};

#[derive(Parser)]
#[command(
    name = "zhuye-import",
    about = "把外部 Harness(Claude Code / Codex / ZCode)历史会话导入为烛夜会话;`config` 子命令一键导入其他 agent 的模型 / MCP 配置",
    after_help = "续跑导入的会话:zhuye exec --resume <thread_id>\n一键导入其他 agent 配置:zhuye-import config"
)]
struct Cli {
    /// 待导入的会话文件或目录(目录时递归收集 .jsonl)。
    paths: Vec<PathBuf>,

    /// 来源种类;`auto` 对 JSONL 逐文件嗅探(ZCode 的 SQLite 来源必须显式 zcode)。
    #[arg(long, default_value = "auto")]
    from: String,

    /// 扫描该来源本机默认安装位置的全部会话(忽略 paths)。
    #[arg(long)]
    all: bool,

    /// 只解析与报告,不落盘。
    #[arg(long)]
    dry_run: bool,

    /// 保留思考链(thinking / reasoning)。
    #[arg(long)]
    include_thinking: bool,

    /// 保留 Harness 注入的环境脚手架(Codex `<environment_context>` 等)。
    #[arg(long)]
    keep_injected: bool,

    /// 保留 Claude Code 子代理(sidechain)行。
    #[arg(long)]
    include_sidechain: bool,

    /// 覆盖同 id 已存在的本地会话(默认跳过)。
    #[arg(long)]
    force: bool,

    /// 机器可读输出:每源一行 JSON(结构见 ImportOutcome / 失败项)。
    #[arg(long)]
    json: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 以 stdio MCP server 常驻(插件 harness-import 的入口)。
    Mcp,
    /// v1.10 功能1:一键导入其他主流本地 agent(Claude Code / Codex /
    /// Gemini / opencode / crush)的模型 / MCP server 配置到烛夜
    /// config.toml。缺省扫描全部已知来源。
    Config {
        /// 限定来源(claude-code / codex / gemini / opencode / crush;
        /// 缺省 = 全部,即「一键」)。
        #[arg(long, default_value = "auto")]
        from: String,

        /// 来源 HOME 根目录(默认 $HOME;脚本 / 测试用)。
        #[arg(long)]
        home: Option<PathBuf>,

        /// 目标 config.toml 路径(默认 ~/.zhuye/config.toml)。
        #[arg(long)]
        output: Option<PathBuf>,

        /// 只报告将导入的内容,不写目标文件。
        #[arg(long)]
        dry_run: bool,

        /// 机器可读输出(单行 JSON 报告)。
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Mcp) => {
            // MCP 模式:stdout 是传输通道,不初始化任何会写 stdout 的设施。
            zhuye_import::mcp_server::run();
            ExitCode::SUCCESS
        }
        Some(Command::Config {
            from,
            home,
            output,
            dry_run,
            json,
        }) => match run_config_import(&from, home, output, dry_run, json) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("zhuye-import config: {e}");
                ExitCode::FAILURE
            }
        },
        None => match run_import(cli) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("zhuye-import: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

/// v1.10 功能1:`config` 子命令实现 —— 扫描 → 解析 → 合并 → (写回)。
fn run_config_import(
    from: &str,
    home: Option<PathBuf>,
    output: Option<PathBuf>,
    dry_run: bool,
    json: bool,
) -> Result<(), String> {
    use zhuye_import::config_import::{self, merge};

    // 来源 HOME:--home > $HOME。
    let home = home
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .ok_or_else(|| "无法确定来源 HOME(设置 $HOME 或传 --home)".to_string())?;

    // 来源集合:auto = 全部;否则按 --from 限定。
    let kinds: Vec<config_import::ConfigSourceKind> = if from == "auto" {
        config_import::ConfigSourceKind::all().to_vec()
    } else {
        let k = config_import::ConfigSourceKind::parse(from).ok_or_else(|| {
            format!(
                "未知 --from '{from}'(可选:claude-code / codex / gemini / opencode / crush / auto)"
            )
        })?;
        vec![k]
    };

    // 解析(单来源失败不阻断 —— 一键语义是尽力而为)。
    let mut imported_list = Vec::new();
    let mut warnings = Vec::new();
    for kind in kinds {
        match config_import::sources::parse_source(kind, &home) {
            Ok(cfg) => imported_list.push(cfg),
            Err(e) => warnings.push(format!("{}: {e}", kind.as_str())),
        }
    }

    // 目标配置:--output > 默认(~/.zhuye/config.toml 口径,经 ZHUYE_HOME)。
    let target = output
        .or_else(zhuye_config::default_config_path)
        .ok_or_else(|| "无法确定目标 config.toml 路径(设置 $HOME 或传 --output)".to_string())?;
    let (mut cfg, load_warnings) = merge::load_target(&target);
    warnings.extend(load_warnings);

    // 合并(dry-run 也先合并内存态,报告与真实执行完全一致)。
    let mut report = merge::MergeReport::default();
    for imported in &imported_list {
        let r = merge::merge_into_config(&mut cfg, imported);
        report.providers_added.extend(r.providers_added);
        report.providers_skipped.extend(r.providers_skipped);
        report.mcp_added.extend(r.mcp_added);
        report.mcp_skipped.extend(r.mcp_skipped);
        report.warnings.extend(r.warnings);
    }
    report.warnings.extend(warnings);

    // 输出。
    if json {
        println!(
            "{}",
            serde_json::json!({
                "target": target.display().to_string(),
                "dry_run": dry_run,
                "providers_added": report.providers_added,
                "providers_skipped": report.providers_skipped,
                "mcp_added": report.mcp_added,
                "mcp_skipped": report.mcp_skipped,
                "warnings": report.warnings,
            })
        );
    } else {
        println!("目标配置:{}", target.display());
        if dry_run {
            println!("(dry-run:以下内容未写入)");
        }
        for label in &report.providers_added {
            println!("  + provider 凭证:{label}");
        }
        for label in &report.providers_skipped {
            println!("  = provider 凭证(已存在,跳过):{label}");
        }
        for name in &report.mcp_added {
            println!("  + mcp server:{name}");
        }
        for name in &report.mcp_skipped {
            println!("  = mcp server(已存在,跳过):{name}");
        }
        for w in &report.warnings {
            eprintln!("  ! {w}");
        }
    }

    // 写回。
    if !dry_run {
        merge::write_target(&target, &cfg)?;
        if !json {
            println!("已写入:{}", target.display());
        }
    }
    Ok(())
}

fn run_import(cli: Cli) -> Result<(), String> {
    let kind = if cli.from == "auto" {
        None
    } else {
        Some(HarnessKind::parse(&cli.from).ok_or_else(|| {
            format!(
                "未知 --from '{}'(可选:claude-code / codex / zcode / auto)",
                cli.from
            )
        })?)
    };
    let opts = ImportOptions {
        include_thinking: cli.include_thinking,
        keep_injected: cli.keep_injected,
        include_sidechain: cli.include_sidechain,
    };
    let base = zhuye_rollout::path::default_base();

    // ── 组装来源清单 ──
    let mut sources: Vec<ImportSource> = Vec::new();
    if cli.all {
        let kind = kind.ok_or("--all 需要显式 --from(zcode SQLite 无法自动嗅探)?")?;
        sources = discover(kind).map_err(|e| e.to_string())?;
        eprintln!("发现 {} 个 {} 来源", sources.len(), kind);
    } else {
        for p in &cli.paths {
            if p.is_file() {
                sources.push(ImportSource::JsonlFile(p.clone()));
            } else if p.is_dir() {
                let mut inner = Vec::new();
                collect_jsonl(p, &mut inner, 0);
                if inner.is_empty() {
                    return Err(format!("目录中没有 .jsonl 文件: {}", p.display()));
                }
                eprintln!("目录 {} 收集到 {} 个 .jsonl", p.display(), inner.len());
                sources.extend(inner);
            } else {
                return Err(format!("路径不存在: {}", p.display()));
            }
        }
    }
    if sources.is_empty() {
        return Err("没有可导入的来源(给 paths 或用 --all)".into());
    }

    let (ok, failed) = import_all(&base, &sources, kind, &opts, cli.force, cli.dry_run);

    // 幂等冲突单列:skip 是预期行为,不算失败(退出码 0)。
    let (skipped, errors): (Vec<_>, Vec<_>) = failed
        .into_iter()
        .partition(|(_, e)| matches!(e, zhuye_import::ImportError::AlreadyExists(_)));

    // ── 输出 ──
    for o in &ok {
        if cli.json {
            println!(
                "{}",
                serde_json::json!({
                    "status": "imported",
                    "source": o.source,
                    "source_id": o.source_id,
                    "thread_id": o.thread_id.to_string(),
                    "path": o.path.as_ref().map(|p| p.display().to_string()),
                    "turns": o.turns,
                    "records": o.records,
                    "warnings": o.warnings.iter().map(|w| w.to_string()).collect::<Vec<_>>(),
                })
            );
        } else {
            println!("{}", o.summary_line());
        }
    }
    for (src, _) in &skipped {
        if cli.json {
            println!(
                "{}",
                serde_json::json!({
                    "status": "skipped",
                    "source": src,
                    "reason": "already-exists",
                })
            );
        } else {
            println!("skipped {src}: 目标会话已存在(加 --force 覆盖)");
        }
    }
    for (src, e) in &errors {
        if cli.json {
            println!(
                "{}",
                serde_json::json!({
                    "status": "failed",
                    "source": src,
                    "error": e.to_string(),
                })
            );
        } else {
            println!("failed {src}: {e}");
        }
    }
    eprintln!(
        "完成:{} 个导入,{} 个跳过,{} 个失败 -> {}",
        ok.len(),
        skipped.len(),
        errors.len(),
        base.display()
    );

    // 有真实错误且无任何成功 → 非零退出(脚本可感知);
    // 全部 skip / 部分失败保持成功(批量语义)。
    if ok.is_empty() && !errors.is_empty() {
        return Err("没有任何来源导入成功".into());
    }
    Ok(())
}

/// 递归收集目录下的 .jsonl(深度上限 8)。
fn collect_jsonl(dir: &std::path::Path, out: &mut Vec<ImportSource>, depth: usize) {
    let mut pending = std::collections::VecDeque::from([(dir.to_path_buf(), depth)]);
    let mut visited = std::collections::HashSet::new();
    while let Some((dir, depth)) = pending.pop_front() {
        if depth > 8 {
            continue;
        }
        let Ok(real_dir) = dir.canonicalize() else {
            continue;
        };
        if !visited.insert(real_dir) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                pending.push_back((path, depth + 1));
            } else if path.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                out.push(ImportSource::JsonlFile(path));
            }
        }
    }
}
