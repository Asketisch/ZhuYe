// TS SDK 绑定层:SDK 客户端自身的行为(不测服务端语义)。
//
// 与能力覆盖层(tests/functional/,Python 原始 wire 打真二进制)互补:
// 本文件驱动**真二进制**,但断言的是 **TS SDK 怎么反应** —— 迭代器何时
// 收尾、handler 怎么被调、close 怎么收尾。
//
// 覆盖(K1–K9,与 tests/sdk-python/test_sdk_real_bin.py 对齐):
//   K1 握手:spawn 返回即就绪;
//   K2 单轮 prompt 文本回流;
//   K3 远程工具:注册 → LLM 调用 → 本地执行 → 回执 → turn_complete;
//   K4 非 turn op 收尾(submit 迭代器以 submission_closed 结束);
//   K5 interrupt 打断在飞 turn(被打断的 turn 收 turn_aborted);
//   K6 close 幂等;
//   K7 坏流(非 JSONL)→ 握手超时抛错;K7b 二进制不存在 → spawn 失败
//      转为可等待的 rejection(而非 uncaught exception 打爆宿主);
//   K8 handler 抛异常 → 转为 is_error=true 的错误回执;
//   K9 全局事件经 nextGlobalEvent 读取,不混入 submission 迭代器。
//
// 为什么还留这些用例:历史证明只有「真二进制」这一层能发现协议漂移 ——
// v1.3 之前两 SDK 迭代器在非 turn op 上永久挂起、TS approve 发
// {type:'approve'} 导致审批永久挂起,这两个缺陷在 mock serve 层都测不出来
// (mock 是自己写的假服务端,不会随真协议演进)。
//
// 运行:tests/run_all.sh sdk-ts,或
//   ZHUYE_BIN=... node --test tests/sdk-ts/real-bin.test.mjs

import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, writeFileSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';

// 直接 import 构建产物 dist(与 scripts/sdk_smoke.sh 同路径约定),
// 避免 vitest 配置;run.sh 会先确保 dist 已构建。
const { ZhuyeAgent } = await import(
  new URL('../../sdks/typescript/dist/index.js', import.meta.url).href
);

const ROOT = new URL('../../', import.meta.url).pathname;
const ZHUYE_BIN = process.env.ZHUYE_BIN ?? join(ROOT, 'target/release/zhuye');

/** 隔离 HOME:预写关闭全部内置 hook 的最小 config.toml(理由同 Python 侧)。 */
const home = mkdtempSync(join(tmpdir(), 'zhuye-ts-home.'));
mkdirSync(join(home, '.zhuye'), { recursive: true });
writeFileSync(
  join(home, '.zhuye', 'config.toml'),
  '# 测试用最小配置:关闭全部内置 hook\n[hooks]\nenabled = []\n'
);
process.env.HOME = home;

/** 构造指向真二进制 + mock provider 的 spawn 选项;写临时 mock 脚本。 */
function makeOptions(scriptLines) {
  const dir = mkdtempSync(join(tmpdir(), 'zhuye-ts-script.'));
  const env = { ZHUYE_MODEL: 'mock' };
  if (scriptLines) {
    const script = join(dir, 'script.jsonl');
    writeFileSync(script, scriptLines.join('\n') + '\n');
    env.ZHUYE_MOCK_SCRIPT = script;
  }
  return { bin: ZHUYE_BIN, env };
}

/** 给 promise 挂墙钟上限:回归导致收尾事件缺失时抛错,绝不挂死套件。 */
function withTimeout(promise, ms, label) {
  let timer;
  const timeoutP = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(label)), ms);
  });
  timer.unref?.();
  return Promise.race([promise, timeoutP]).finally(() => clearTimeout(timer));
}

describe('TS SDK 绑定层 × 真二进制', () => {
  it('K1: spawn 完成 session_configured 握手', async () => {
    const agent = await ZhuyeAgent.spawn(makeOptions());
    await agent.close();
  });

  it('K2: 单轮 prompt 文本回流', async () => {
    const agent = await ZhuyeAgent.spawn(
      makeOptions(['{"type":"text","text":"ts-e2e-reply"}'])
    );
    try {
      assert.equal(await agent.prompt('第一轮'), 'ts-e2e-reply');
    } finally {
      await agent.close();
    }
  });

  it('K15: 并发提交的事件分别路由到各自迭代器', async () => {
    const agent = await ZhuyeAgent.spawn(makeOptions([
      '{"type":"text","text":"并发回复甲"}',
      '{"type":"text","text":"并发回复乙"}',
    ]));
    try {
      const first = await agent.submit('第一条');
      const second = await agent.submit('第二条');
      const collect = async (stream) => {
        const events = [];
        for await (const event of stream) events.push(event);
        return events;
      };
      const streams = await withTimeout(Promise.all([collect(first), collect(second)]), 30000, '并发提交未收尾');
      const ids = streams.map((events) => new Set(events.map((event) => event.id)));
      assert.equal(ids[0].size, 1);
      assert.equal(ids[1].size, 1);
      assert.notEqual([...ids[0]][0], [...ids[1]][0]);
      const replies = streams.map((events) => {
        assert.equal(events.filter((event) => event.msg.type === 'turn_complete').length, 1);
        return events.filter((event) => event.msg.type === 'agent_message_delta')
          .map((event) => event.msg.delta).join('');
      });
      assert.deepEqual(new Set(replies), new Set(['并发回复甲', '并发回复乙']));
    } finally {
      await agent.close();
    }
  });

  it('K16: 回合中进程崩溃不会返回成功答案', async () => {
    let started;
    let release;
    const handlerStarted = new Promise((resolve) => { started = resolve; });
    const handlerRelease = new Promise((resolve) => { release = resolve; });
    const agent = await ZhuyeAgent.spawn(makeOptions([
      '{"type":"tool_call","name":"slow_tool","args":{}}',
    ]));
    try {
      await agent.registerTool('slow_tool', '等待', { type: 'object' }, async () => {
        started();
        await handlerRelease;
        return { content: [], is_error: false, metadata: {}, elapsed_ms: 0 };
      });
      const result = agent.prompt('调用工具');
      await withTimeout(handlerStarted, 10000, '工具没有启动');
      agent.childProcess.kill();
      await assert.rejects(withTimeout(result, 10000, '断线后 prompt 未结束'), /before turn_complete/);
    } finally {
      release();
      await agent.close();
    }
  });

  it('K3: registerTool → LLM 调用 → 客户端执行 → 回执 → turn_complete', async () => {
    const calls = [];
    const agent = await ZhuyeAgent.spawn(
      makeOptions([
        '{"type":"tool_call","name":"sdk_echo","args":{"q":"ping"}}',
        '{"type":"text","text":"tool-done"}',
      ])
    );
    try {
      await agent.registerTool('sdk_echo', 'echo back', { type: 'object' }, (args) => {
        calls.push(args);
        return {
          content: [{ type: 'text', text: `echo:${args.q}` }],
          is_error: false,
          metadata: {},
          elapsed_ms: 0,
        };
      });
      let sawComplete = false;
      for await (const ev of await agent.submit('调工具')) {
        if (ev.msg.type === 'turn_complete') {
          sawComplete = true;
          break;
        }
      }
      assert.deepEqual(calls, [{ q: 'ping' }]);
      assert.ok(sawComplete, '未见 turn_complete');
    } finally {
      await agent.close();
    }
  });

  it('K13: 工具图片回执经 SDK 往返', async () => {
    const calls = [];
    const agent = await ZhuyeAgent.spawn(makeOptions([
      '{"type":"tool_call","name":"sdk_image","args":{}}',
      '{"type":"text","text":"done"}',
    ]));
    try {
      await agent.registerTool('sdk_image', '图片', { type: 'object' }, (args) => {
        calls.push(args);
        return { content: [{ type: 'image', data: [137, 80, 78, 71], mime_type: 'image/png' }], is_error: false, metadata: {}, elapsed_ms: 0 };
      });
      const events = [];
      for await (const event of await agent.submit('图片')) events.push(event);
      assert.deepEqual(calls, [{}]);
      const end = events.find((event) => event.msg.type === 'tool_call_end');
      assert.deepEqual(end?.msg.output.content, [{ type: 'image', data: [137, 80, 78, 71], mime_type: 'image/png' }]);
      assert.ok(events.some((event) => event.msg.type === 'turn_complete'));
    } finally {
      await agent.close();
    }
  });

  it('K4: 非 turn op 的迭代器以 submission_closed 收尾', async () => {
    // 没有 submission_closed,SDK 迭代器会永久挂起(v1.3 起 serve 在
    // per-turn 通道排空后发这条收尾标记)。
    const agent = await ZhuyeAgent.spawn(
      makeOptions(['{"type":"text","text":"t1"}'])
    );
    try {
      const types = [];
      for await (const ev of await agent.submit({ type: 'set_effort', effort: 'high' })) {
        types.push(ev.msg.type);
      }
      assert.deepEqual(types, ['submission_closed'], `收尾异常:${types}`);
    } finally {
      await agent.close();
    }
  });

  it('K14: rescan_extensions additive Op → extensions_rescanned 字段级', async () => {
    // v1.x additive:Op::RescanExtensions(skills 重扫)→
    // extensions_rescanned(skills 计数 / commands=false)+ submission_closed
    // 收尾;绑定层对新 Op / 新事件的双向透传,与 V11(serve 直发)互补。
    const agent = await ZhuyeAgent.spawn(
      makeOptions(['{"type":"text","text":"t1"}'])
    );
    try {
      const types = [];
      let evt = null;
      for await (const ev of await agent.submit({
        type: 'rescan_extensions', skills: true, commands: false,
      })) {
        types.push(ev.msg.type);
        if (ev.msg.type === 'extensions_rescanned') evt = ev.msg;
      }
      assert.ok(types.includes('extensions_rescanned'), `缺回执事件:${types}`);
      assert.ok(Number.isInteger(evt.skills) && evt.skills >= 0, `skills 字段:${evt}`);
      assert.equal(evt.commands, false, `commands 位恒 false:${evt}`);
      assert.equal(types.at(-1), 'submission_closed', `收尾异常:${types}`);
    } finally {
      await agent.close();
    }
  });

  it('K4b: set_effort 新档位 max/ultra 经真二进制透传', async () => {
    // v1.10 功能4:additive 新档位;引擎与 high 同型静默收尾,SDK 与 serve
    // 都不得因新增字面量崩溃(协议 additive 变更的绑定层回归);
    // 随后验证支持的回合设置可由 SDK 发送并正常收尾。
    const agent = await ZhuyeAgent.spawn(
      makeOptions(['{"type":"text","text":"t1"}'])
    );
    try {
      for (const effort of ['max', 'ultra']) {
        const types = [];
        for await (const ev of await agent.submit({ type: 'set_effort', effort })) {
          types.push(ev.msg.type);
        }
        assert.deepEqual(types, ['submission_closed'], `收尾异常:${types}`);
      }
      const turnTypes = [];
      for await (const ev of await agent.submit({
        type: 'user_input',
        items: [{ type: 'text', text: 'hi' }],
        thread_settings: { approval_policy: 'deny' },
      })) {
        turnTypes.push(ev.msg.type);
      }
      assert.ok(turnTypes.includes('turn_complete'), `turn 未收尾:${turnTypes}`);
    } finally {
      await agent.close();
    }
  });

  it('K10: side-channel 查询 Op 返回 status 快照 + submission_closed 收尾', async () => {
    // WP9 新能力面:exec 已装配 side-channel driver(WP13),查询有真实
    // 应答(与 K4 的 set_effort 静默不同)。纯逻辑层同型用例在
    // sdks/typescript/tests/protocol.test.ts(mock serve)。
    const agent = await ZhuyeAgent.spawn(
      makeOptions(['{"type":"text","text":"t1"}'])
    );
    try {
      const types = [];
      let status;
      for await (const ev of await agent.submit({ type: 'query_side_channels' })) {
        types.push(ev.msg.type);
        if (ev.msg.type === 'side_channel_status') status = ev.msg;
      }
      assert.equal(types.at(-1), 'submission_closed', `收尾异常:${types}`);
      assert.ok(Array.isArray(status?.channels), `channels 应为数组:${JSON.stringify(status)}`);
    } finally {
      await agent.close();
    }
  });

  it('K11: side-channel 终态进入全局事件队列', async () => {
    const agent = await ZhuyeAgent.spawn(makeOptions(['{"type":"text","text":"side response"}']));
    try {
      await agent.nextGlobalEvent({ timeoutMs: 5000 });
      const events = [];
      for await (const event of await agent.submit({ type: 'start_side_channel', agent: 'default', prompt: 'p' })) {
        events.push(event);
      }
      assert.equal(events.at(-1).msg.type, 'submission_closed');
      const event = await agent.nextGlobalEvent({ timeoutMs: 10000 });
      assert.equal(event.id, '');
      assert.equal(event.msg.type, 'side_channel_finished');
    } finally {
      await agent.close();
    }
  });

  it('K12: 侧通道审批经全局队列和 SDK 回执', async () => {
    const isolatedHome = mkdtempSync(join(tmpdir(), 'zhuye-ts-approval.'));
    mkdirSync(join(isolatedHome, '.zhuye'), { recursive: true });
    writeFileSync(join(isolatedHome, '.zhuye', 'config.toml'),
      '[hooks]\nenabled = []\n[[permissions.rule]]\ntool = "bash"\naction = "ask"\n');
    const opts = makeOptions([
      '{"type":"tool_call","name":"bash","args":{"cmd":"echo test"}}',
      '{"type":"tool_call","name":"ask_user","args":{"prompt":"name?"}}',
      '{"type":"text","text":"done"}',
    ]);
    opts.env.HOME = isolatedHome;
    opts.env.ZHUYE_APPROVALS = '1';
    const agent = await ZhuyeAgent.spawn(opts);
    try {
      await agent.nextGlobalEvent({ timeoutMs: 5000 });
      for await (const _event of await agent.submit({ type: 'start_side_channel', agent: 'default', prompt: 'p' })) {
        // 排空提交事件,审批请求从全局队列读取。
      }
      const approval = await agent.nextGlobalEvent({ timeoutMs: 10000 });
      assert.equal(approval.id, '');
      assert.equal(approval.msg.type, 'approval_request');
      await agent.approve(approval.msg.request_id, 'approve');
      const question = await agent.nextGlobalEvent({ timeoutMs: 10000 });
      assert.equal(question.msg.type, 'ask_user_input');
      for await (const _event of await agent.submit({ type: 'ask_user_input_response', id: question.msg.request_id, text: 'answer' })) {
        // 排空回执。
      }
      const finished = await agent.nextGlobalEvent({ timeoutMs: 15000 });
      assert.equal(finished.msg.type, 'side_channel_finished');
    } finally {
      await agent.close();
    }
  });

  it('K6: close 幂等 + 子进程优雅退出', async () => {
    const agent = await ZhuyeAgent.spawn(makeOptions());
    await agent.close();
    await agent.close();
  });

  it('K5: interrupt 打断在飞 turn', async () => {
    // wire 语义(与 Python K5 实测对齐):turn_aborted 落在被打断的
    // turn 通道上;interrupt 这条 submission 自己的通道只收到
    // submission_closed 收尾标记。
    let handlerStartedResolve;
    const handlerStarted = new Promise((resolve) => {
      handlerStartedResolve = resolve;
    });
    let releaseResolve;
    const releasePromise = new Promise((resolve) => {
      releaseResolve = resolve;
    });

    const agent = await ZhuyeAgent.spawn(
      makeOptions([
        '{"type":"tool_call","name":"slow_tool","args":{}}',
        '{"type":"text","text":"after"}',
      ])
    );
    try {
      await agent.registerTool(
        'slow_tool',
        'block until released',
        { type: 'object' },
        async () => {
          handlerStartedResolve();
          await releasePromise;
          return {
            content: [{ type: 'text', text: 'slow result' }],
            is_error: false,
            metadata: {},
            elapsed_ms: 0,
          };
        }
      );
      // 背景消费 turn 事件流;被打断的 turn 应以 turn_aborted 收尾。
      const turnEvents = [];
      const turnDone = (async () => {
        for await (const ev of await agent.submit('调慢工具')) {
          turnEvents.push(ev);
        }
      })();
      await withTimeout(handlerStarted, 10_000, 'handler 未被调用');
      // interrupt 自己的通道:只应收到收尾标记。
      const intrTypes = [];
      for await (const ev of await agent.submit(
        { type: 'interrupt', child_id: null },
        { submissionId: 'intr-1' }
      )) {
        intrTypes.push(ev.msg.type);
      }
      assert.deepEqual(
        intrTypes,
        ['submission_closed'],
        `interrupt 通道应只有收尾标记:${JSON.stringify(intrTypes)}`
      );
      releaseResolve();
      await withTimeout(turnDone, 10_000, '被打断的 turn 未收尾');
      const turnTypes = turnEvents.map((e) => e.msg.type);
      assert.ok(
        turnTypes.includes('turn_aborted'),
        `被打断的 turn 未收到 turn_aborted:${JSON.stringify(turnTypes)}`
      );
    } finally {
      releaseResolve?.();
      await agent.close();
    }
  });

  it('K7: 非 JSONL 输出 → 握手失败抛错而非挂死', async () => {
    // 与 Python K7 同构:/bin/sh 吐非 JSONL → 握手超时以异常呈现。
    const child = spawn('/bin/sh', ['-c', 'echo not-jsonl && sleep 5'], {
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    const agent = await ZhuyeAgent.fromStdio(child.stdin, child.stdout, 2000);
    try {
      await assert.rejects(() => agent.submit('hi'), /handshake timed out/);
    } finally {
      child.kill();
      await agent.close();
    }
  });

  it('K7b: 二进制不存在 → spawn 失败转为可等待 rejection', async () => {
    // ChildProcess 的 error 事件无监听者会以 uncaught exception 打爆
    // 宿主进程;SDK 必须转为握手失败(经 await ready 呈现给调用方)。
    const agent = await ZhuyeAgent.spawn({
      bin: '/nonexistent/zhuye-e2e-missing-bin',
      handshakeTimeoutMs: 5000,
    });
    await assert.rejects(() => agent.submit('hi'), /failed to start/);
    await agent.close();
  });

  it('K8: handler 抛异常 → 转为 is_error=true 的错误回执', async () => {
    // 依据 sdks/PROTOCOL.md §5:handler 异常要转成错误回执,让 LLM
    // 看到错误而不是挂死。
    const agent = await ZhuyeAgent.spawn(
      makeOptions([
        '{"type":"tool_call","name":"boom_tool","args":{}}',
        '{"type":"text","text":"after-boom"}',
      ])
    );
    try {
      await agent.registerTool(
        'boom_tool',
        'always raises',
        { type: 'object' },
        () => {
          throw new Error('handler-boom');
        }
      );
      const evs = [];
      for await (const ev of await agent.submit('调会崩的工具')) evs.push(ev);
      const end = evs.find((e) => e.msg.type === 'tool_call_end');
      assert.ok(end, `未见 tool_call_end:${JSON.stringify(evs.map((e) => e.msg.type))}`);
      assert.equal(end.msg.is_error, true, `应回错误:${JSON.stringify(end.msg)}`);
      const text = (end.msg.output?.content ?? [])
        .map((c) => c.text ?? '')
        .join('');
      assert.ok(text.includes('handler-boom'), `错误文本未透传:${text}`);
      assert.ok(
        evs.some((e) => e.msg.type === 'turn_complete'),
        'turn 未收尾'
      );
    } finally {
      await agent.close();
    }
  });

  it('K9: 全局事件经 nextGlobalEvent 读取,不混入 submission 迭代器', async () => {
    const agent = await ZhuyeAgent.spawn(makeOptions());
    try {
      // 握手事件本身也在全局队列(对齐 Python next_global_event 语义)。
      const handshake = await agent.nextGlobalEvent({ timeoutMs: 5000 });
      assert.equal(handshake.id, '');
      assert.equal(handshake.msg.type, 'session_configured');
      // 非 turn op:迭代器以 submission_closed 收尾。
      const types = [];
      for await (const ev of await agent.submit({
        type: 'set_permission_mode',
        mode: 'plan',
      })) {
        types.push(ev.msg.type);
      }
      assert.equal(types.at(-1), 'submission_closed', `收尾异常:${JSON.stringify(types)}`);
      const event = await agent.nextGlobalEvent({ timeoutMs: 5000 });
      assert.equal(event.id, '');
      assert.equal(event.msg.type, 'permission_mode_changed');
    } finally {
      await agent.close();
    }
  });

  it('K10: plugins 选项显式加载插件并上报 plugin_loaded', async () => {
    // `plugins: ['demo@inline']` 经 --plugin 传给 serve(覆盖 config,
    // 而非配置驱动);启动时上报 plugin_loaded 全局事件。
    // 依据:crates/runtime/zhuye-exec/src/headless.rs resolve_enabled_plugins、
    // sdks/typescript/src/agent.ts serveArgs。
    const pluginDir = join(ROOT, 'plugins', 'plugin-demo');
    if (!existsSync(pluginDir)) {
      // 仓库不完整时跳过(node:test 无原生 skip,以断言占位)。
      assert.ok(true, 'plugins/plugin-demo 不存在,跳过');
      return;
    }
    // 先把 demo 插件装入隔离 HOME(等价 zhuye plugin install)。
    const install = spawnSync(ZHUYE_BIN, [
      'plugin',
      'install',
      pluginDir,
      '--scope',
      'user',
    ]);
    assert.equal(
      install.status,
      0,
      `plugin install 失败:${install.stderr?.toString()}`
    );

    const agent = await ZhuyeAgent.spawn({
      ...makeOptions(),
      plugins: ['demo@inline'],
    });
    try {
      // plugin_loaded 在 bootstrap 期发出,轮询全局队列(上限 ~15s)。
      let loaded = null;
      for (let i = 0; i < 15 && loaded === null; i++) {
        const ev = await agent
          .nextGlobalEvent({ timeoutMs: 1000 })
          .catch(() => null);
        if (ev && ev.msg.type === 'plugin_loaded') loaded = ev.msg;
      }
      assert.ok(loaded, '未收到 plugin_loaded 全局事件');
      assert.equal(loaded.plugin, 'demo@inline', `插件 id 不符:${JSON.stringify(loaded)}`);
      assert.ok(
        (loaded.command_count ?? 0) >= 1,
        `应有 demo:hello 命令:${JSON.stringify(loaded)}`
      );
    } finally {
      await agent.close();
    }
  });
});
