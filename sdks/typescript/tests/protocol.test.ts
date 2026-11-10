//! ZhuyeAgent 协议回路测试:用一个轻量 mock serve 进程驱动协议。
//!
//! mock serve 由 Node 直接跑一个 stdin→stdout 回显脚本(模拟 zhuye
//! serve 的 JSONL 协议),无需 spawn `zhuye` 二进制,测试完全离线。
//!
//! 行为契约:
//! - 收到任何 stdin 一行 → emit `session_configured`(`id=""`)回应;
//! - 之后把每行 stdin 解析为 Submission,emit 一个匹配 submission.id
//!   的 turn_complete 事件 + agent_message_delta。
//! - 收到 `register_tools` op → 不发任何事件(注册被静默接受);
//! - 收到 `tool_execution_response` op → 不发任何事件(回执被静默接受);
//! - 收到 `shutdown` op → 发 shutdown_complete 后退出。

import { spawn, ChildProcessWithoutNullStreams } from 'node:child_process';
import { Readable, Writable, PassThrough } from 'node:stream';

import { describe, expect, it, beforeEach, afterEach, expectTypeOf } from 'vitest';

import {
  EVENT_ID_NONE,
  Event,
  EventMsg,
  ReasoningEffort,
  ZhuyeAgent,
} from '../src/index.js';

import type { TurnComplete, AbortReason, PlanApprovalChoice, ReviewDecision, ToolCallOutputDelta, ThreadSettings } from '../src/index.js';

it.each([-1, Number.NaN, Number.POSITIVE_INFINITY, 2_147_483_648])(
  'prompt 拒绝无效超时值 %s',
  async (timeoutMs) => {
    const stdout = new PassThrough();
    const writes: string[] = [];
    const stdin = new Writable({
      write(chunk, _encoding, callback) {
        writes.push(chunk.toString());
        callback();
      },
    });
    const agent = await ZhuyeAgent.fromStdio(stdin, stdout);
    stdout.write(JSON.stringify({ id: '', msg: { type: 'session_configured' } }) + '\n');
    try {
      await expect(agent.prompt('无效参数', { timeoutMs })).rejects.toThrow(RangeError);
      expect(writes).toHaveLength(0);
    } finally {
      stdout.end();
      await agent.close();
    }
  },
);

it('prompt 超时仅结束本地等待,迟到事件不影响并发回合', async () => {
  const stdout = new PassThrough();
  const writes: Array<{ id: string; op: { type: string } }> = [];
  const stdin = new Writable({
    write(chunk, _encoding, callback) {
      const sub = JSON.parse(chunk.toString());
      writes.push(sub);
      if (sub.op.type === 'user_input' && writes.filter((s) => s.op.type === 'user_input').length === 2) {
        stdout.write(JSON.stringify({ id: sub.id, msg: { type: 'agent_message_delta', delta: '并发完成' } }) + '\n');
        stdout.write(JSON.stringify({ id: sub.id, msg: { type: 'turn_complete', turn_id: 't', status: 'success' } }) + '\n');
      }
      callback();
    },
  });
  const agent = await ZhuyeAgent.fromStdio(stdin, stdout);
  stdout.write(JSON.stringify({ id: '', msg: { type: 'session_configured' } }) + '\n');
  try {
    const timedOut = agent.prompt('等待', { timeoutMs: 40 });
    const complete = agent.prompt('并发', { timeoutMs: 1000 });
    expect(await complete).toBe('并发完成');
    await expect(timedOut).rejects.toThrow('prompt timed out after 40ms');
    const first = writes.find((sub) => sub.op.type === 'user_input')!;
    stdout.write(JSON.stringify({ id: first.id, msg: { type: 'agent_message_delta', delta: '迟到' } }) + '\n');
    stdout.write(JSON.stringify({ id: first.id, msg: { type: 'turn_complete', turn_id: 'old', status: 'success' } }) + '\n');
    expect(writes.some((sub) => sub.op.type === 'interrupt')).toBe(false);
    expect((agent as unknown as { eventListeners: Map<string, unknown> }).eventListeners.size).toBe(0);
  } finally {
    stdout.end();
    await agent.close();
  }
});

it('prompt 在握手超时后不会迟到提交', async () => {
  const stdout = new PassThrough();
  const writes: string[] = [];
  const stdin = new Writable({
    write(chunk, _encoding, callback) {
      writes.push(chunk.toString());
      callback();
    },
  });
  const agent = await ZhuyeAgent.fromStdio(stdin, stdout);
  try {
    await expect(agent.prompt('等待握手', { timeoutMs: 30 })).rejects.toThrow('prompt timed out');
    stdout.write(JSON.stringify({ id: '', msg: { type: 'session_configured' } }) + '\n');
    await agent.nextGlobalEvent({ timeoutMs: 1000 });
    await new Promise((resolve) => setImmediate(resolve));
    expect(writes).toHaveLength(0);
  } finally {
    stdout.end();
    await agent.close();
  }
});

it('prompt 在写入回调挂起时超时并清理监听', async () => {
  const stdout = new PassThrough();
  let finishWrite: (() => void) | undefined;
  const stdin = new Writable({
    write(chunk, _encoding, callback) {
      const sub = JSON.parse(chunk.toString());
      if (sub.op.type === 'user_input') finishWrite = callback;
      else callback();
    },
  });
  const agent = await ZhuyeAgent.fromStdio(stdin, stdout);
  stdout.write(JSON.stringify({ id: '', msg: { type: 'session_configured' } }) + '\n');
  try {
    await expect(agent.prompt('写入挂起', { timeoutMs: 40 })).rejects.toThrow('prompt timed out');
    expect(finishWrite).toBeDefined();
    expect((agent as unknown as { eventListeners: Map<string, unknown> }).eventListeners.size).toBe(0);
    finishWrite?.();
    finishWrite = undefined;
    await new Promise((resolve) => setImmediate(resolve));
    expect((agent as unknown as { eventListeners: Map<string, unknown> }).eventListeners.size).toBe(0);
  } finally {
    finishWrite?.();
    stdout.end();
    await agent.close();
  }
});

it('写入尚未完成时收到的响应仍属于对应 submission', async () => {
  const stdout = new PassThrough();
  const stdin = new Writable({
    write(chunk, _encoding, callback) {
      const sub = JSON.parse(chunk.toString());
      if (sub.op.type === 'user_input') {
        stdout.write(JSON.stringify({ id: sub.id, msg: { type: 'agent_message_delta', delta: '快速回复' } }) + '\n');
        stdout.write(JSON.stringify({ id: sub.id, msg: { type: 'turn_complete', turn_id: 't', status: 'success' } }) + '\n');
        setTimeout(callback, 50);
      } else callback();
    },
  });
  const agent = await ZhuyeAgent.fromStdio(stdin, stdout);
  stdout.write(JSON.stringify({ id: '', msg: { type: 'session_configured' } }) + '\n');
  try {
    const result = await Promise.race([
      agent.prompt('你好'),
      new Promise<never>((_, reject) => setTimeout(() => reject(new Error('响应丢失')), 500)),
    ]);
    expect(result).toBe('快速回复');
  } finally {
    stdout.end();
    await agent.close();
  }
});

it('stdout 关闭时唤醒所有并发的全局事件读取者', async () => {
  const stdin = new PassThrough();
  const stdout = new PassThrough();
  const agent = await ZhuyeAgent.fromStdio(stdin, stdout);
  stdout.write(JSON.stringify({ id: '', msg: { type: 'session_configured' } }) + '\n');
  try {
    expect((await agent.nextGlobalEvent({ timeoutMs: 500 }))?.msg.type).toBe('session_configured');
    const waiters = [agent.nextGlobalEvent({ timeoutMs: 500 }), agent.nextGlobalEvent({ timeoutMs: 500 })];
    stdout.end();
    expect(await Promise.all(waiters)).toEqual([null, null]);
  } finally {
    await agent.close();
  }
});

it('prompt 在收到部分回复后断线时抛错', async () => {
  const stdout = new PassThrough();
  const stdin = new Writable({
    write(chunk, _encoding, callback) {
      const sub = JSON.parse(chunk.toString());
      if (sub.op.type === 'user_input') {
        stdout.write(JSON.stringify({ id: sub.id, msg: { type: 'agent_message_delta', delta: '部分回复' } }) + '\n');
        stdout.end();
      }
      callback();
    },
  });
  const agent = await ZhuyeAgent.fromStdio(stdin, stdout);
  stdout.write(JSON.stringify({ id: '', msg: { type: 'session_configured' } }) + '\n');
  try {
    await expect(agent.prompt('你好')).rejects.toThrow('before turn_complete');
  } finally {
    await agent.close();
  }
});

it('prompt 收到部分回复和错误后仍可继续', async () => {
  const stdout = new PassThrough();
  let count = 0;
  const stdin = new Writable({
    write(chunk, _encoding, callback) {
      const sub = JSON.parse(chunk.toString());
      if (sub.op.type === 'user_input') {
        count++;
        if (count === 1) {
          stdout.write(JSON.stringify({ id: sub.id, msg: { type: 'agent_message_delta', delta: '部分回复' } }) + '\n');
          stdout.write(JSON.stringify({ id: sub.id, msg: { type: 'error', code: 'SSE_PARSE_ERROR', message: '流中断' } }) + '\n');
          stdout.write(JSON.stringify({ id: sub.id, msg: { type: 'submission_closed' } }) + '\n');
        } else {
          stdout.write(JSON.stringify({ id: sub.id, msg: { type: 'agent_message_delta', delta: '下一轮' } }) + '\n');
          stdout.write(JSON.stringify({ id: sub.id, msg: { type: 'turn_complete', turn_id: 't', status: 'success' } }) + '\n');
        }
      }
      callback();
    },
  });
  const agent = await ZhuyeAgent.fromStdio(stdin, stdout);
  stdout.write(JSON.stringify({ id: '', msg: { type: 'session_configured' } }) + '\n');
  try {
    await expect(agent.prompt('第一轮')).rejects.toThrow('SSE_PARSE_ERROR');
    expect(await agent.prompt('第二轮')).toBe('下一轮');
  } finally {
    stdout.end();
    await agent.close();
  }
});

it('关闭后提交立即报错而不是返回永远等待的迭代器', async () => {
  const stdin = new PassThrough();
  const stdout = new PassThrough();
  const agent = await ZhuyeAgent.fromStdio(stdin, stdout);
  stdout.write(JSON.stringify({ id: '', msg: { type: 'session_configured' } }) + '\n');
  await agent.nextGlobalEvent({ timeoutMs: 500 });
  await agent.close();
  await expect(agent.submit('你好')).rejects.toThrow('agent is closed');
  stdout.end();
});

it('stdout 断开后提交立即报错', async () => {
  const stdin = new PassThrough();
  const stdout = new PassThrough();
  const agent = await ZhuyeAgent.fromStdio(stdin, stdout);
  stdout.write(JSON.stringify({ id: '', msg: { type: 'session_configured' } }) + '\n');
  await agent.nextGlobalEvent({ timeoutMs: 500 });
  stdout.end();
  expect(await agent.nextGlobalEvent({ timeoutMs: 500 })).toBeNull();
  await expect(agent.submit('你好')).rejects.toThrow('agent is closed');
  await agent.close();
});

it('公开类型与 Rust wire 值一致', () => {
  expectTypeOf<ThreadSettings['approval_policy']>().toEqualTypeOf<'auto' | 'prompt' | 'deny' | null | undefined>();
  expectTypeOf<ThreadSettings['sandbox_policy']>().toEqualTypeOf<'workspace_only' | 'os_sandbox' | 'full_access' | null | undefined>();
  expectTypeOf<ThreadSettings['max_tool_concurrency']>().toEqualTypeOf<number | null | undefined>();
  expectTypeOf<TurnComplete['status']>().toEqualTypeOf<'success' | 'max_iterations' | 'stopped' | 'token_budget_exceeded'>();
  expectTypeOf<AbortReason>().toEqualTypeOf<{ type: 'user_interrupt' } | { type: 'shutdown' } | { type: 'error'; code: string; message: string }>();
  expectTypeOf<PlanApprovalChoice>().toEqualTypeOf<'auto_mode' | 'manual_approve' | 'revise'>();
  expectTypeOf<ReviewDecision>().toEqualTypeOf<'approve' | 'approve_for_session' | { deny: { reason: string } }>();
  expectTypeOf<ToolCallOutputDelta>().toEqualTypeOf<{ type: 'tool_call_output_delta'; call_id: string; delta: string; is_stderr: boolean }>();
  // v1.10 功能4:思考强度阶梯扩到 max / ultra(变体级 additive,旧值不变)。
  expectTypeOf<ReasoningEffort>().toEqualTypeOf<'low' | 'medium' | 'high' | 'max' | 'ultra'>();
});

/**
 * spawn 一个本地 Node "mock serve" 子进程:读 stdin JSONL、写 stdout JSONL。
 *
 * 把"收到 register_tools / tool_execution_response 不回事件"这一段
 * 行为作为契约,SDK 测试可以放心 assert:
 * - 写入 stdin 后收到对应 turn_complete;
 * - registerTools 后不会立刻被回执事件淹没;
 * - shutdown 后子进程优雅退出。
 */
/** spawn 一个本地 Node "mock serve" 子进程:读 stdin JSONL、写 stdout JSONL。
 *
 * 启动后**立即** emit `session_configured`(不依赖 stdin),便于 agent
 * 握手后再处理第一条 submission。后续收到 register_tools /
 * tool_execution_response 静默接受;user_input → emit turn 三件套;
 * shutdown → emit shutdown_complete 并 exit。
 */
function spawnMockServe(): ChildProcessWithoutNullStreams {
  const script = `
    process.stdout.write(JSON.stringify({
      id: '',
      msg: {
        type: 'session_configured',
        session_id: 'mock-session',
        model: 'mock/mock-1',
        provider: 'mock',
        approval_policy: 'auto',
        sandbox_policy: 'workspace_only',
      },
    }) + '\\n');
    const readline = require('node:readline');
    const rl = readline.createInterface({ input: process.stdin });
    rl.on('line', (line) => {
      let sub;
      try { sub = JSON.parse(line); } catch { return; }
      const opType = sub.op?.type;
      if (opType === 'shutdown') {
        process.stdout.write(JSON.stringify({
          id: sub.id, msg: { type: 'shutdown_complete' },
        }) + '\\n');
        process.exit(0);
        return;
      }
      if (opType === 'register_tools' || opType === 'tool_execution_response') {
        return;
      }
      // v1.x:side-channel 查询 Op(非 turn)—— 回 status 快照后收尾。
      if (opType === 'rescan_extensions') {
        const skills = sub.op.skills ? 12 : 0;
        stdout.write(JSON.stringify({ id: sub.id, msg: {
          type: 'extensions_rescanned', skills, commands: false,
        } }) + '\\n');
        stdout.write(JSON.stringify({ id: sub.id, msg: { type: 'submission_closed' } }) + '\\n');
        return;
      }
      if (opType === 'query_side_channels') {
        process.stdout.write(JSON.stringify({
          id: sub.id, msg: {
            type: 'side_channel_status', channels: [
              { id: 'side-00000001', agentName: 'a', prompt: 'p',
                startedAtMs: 1, status: 'running', durationMs: null },
            ],
          },
        }) + '\\n');
        process.stdout.write(JSON.stringify({
          id: sub.id, msg: { type: 'submission_closed' },
        }) + '\\n');
        return;
      }
      if (opType === 'user_input' || opType === 'interrupt') {
        process.stdout.write(JSON.stringify({
          id: sub.id, msg: { type: 'turn_started', turn_id: 't', user_message_id: 'm' },
        }) + '\\n');
        process.stdout.write(JSON.stringify({
          id: sub.id, msg: { type: 'agent_message_delta', delta: 'mock-reply' },
        }) + '\\n');
        // v1.5 additive 变体:工具参数增量(SDK 应原样透传给客户端)。
        process.stdout.write(JSON.stringify({
          id: sub.id, msg: {
            type: 'tool_call_args_delta', call_id: 'c1', delta: '{"text":"pi',
          },
        }) + '\\n');
        process.stdout.write(JSON.stringify({
          id: sub.id, msg: { type: 'tool_call_output_delta', call_id: 'c1', delta: 'preview', is_stderr: true },
        }) + '\\n');
        process.stdout.write(JSON.stringify({
          id: sub.id, msg: { type: 'tool_call_end', call_id: 'image-call', is_error: false,
            output: { content: [{ type: 'image', data: [137, 80, 78, 71], mime_type: 'image/png' }], is_error: false, metadata: {}, elapsed_ms: 0 } },
        }) + '\\n');
        process.stdout.write(JSON.stringify({
          id: sub.id, msg: {
            type: 'turn_complete', turn_id: 't', status: 'success',
            usage: { input_tokens: 0, output_tokens: 1 },
          },
        }) + '\\n');
      }
    });
  `;
  const child = spawn(process.execPath, ['-e', script], { stdio: ['pipe', 'pipe', 'pipe'] });
  // 把 mock serve 的 stderr 重定向到当前 stderr(可观测,避免泄漏);
  // 实际脚本不写 stderr,这里只是兜底。
  child.stderr.on('data', (c: Buffer) => process.stderr.write(c));
  return child;
}

describe('ZhuyeAgent 协议回路(mock serve 驱动)', () => {
  let agent: ZhuyeAgent;
  let child: ChildProcessWithoutNullStreams;

  beforeEach(async () => {
    child = spawnMockServe();
    // 直接喂 stdin / stdout —— 等同于 ZhuyeAgent.spawn 但跳过 spawn。
    const stdin = child.stdin as Writable;
    const stdout = child.stdout as Readable;
    agent = await ZhuyeAgent.fromStdio(stdin, stdout, 5000);
  }, 10_000);

  afterEach(async () => {
    try {
      await agent.close();
    } catch {
      // 忽略关闭阶段双工关错。
    }
    if (!child.killed) child.kill();
  });

  it('prompt 高层 API 聚合 delta 并返回', async () => {
    expect(await agent.prompt('hi')).toBe('mock-reply');
  }, 10_000);

  it('工具图片事件透传原始字节', async () => {
    const events = [];
    for await (const event of await agent.submit({ type: 'user_input', items: [{ type: 'text', text: 'hi' }] })) {
      events.push(event);
    }
    const end = events.find((event) => event.msg.type === 'tool_call_end');
    expect(end?.msg.type).toBe('tool_call_end');
    if (end?.msg.type !== 'tool_call_end') throw new Error('缺少工具结果');
    expect(end.msg.output.content).toEqual([{ type: 'image', data: [137, 80, 78, 71], mime_type: 'image/png' }]);
    expect(events.at(-1)?.msg.type).toBe('turn_complete');
  }, 10_000);

  it('registerTools 静默接受(不污染 turn 事件流)', async () => {
    await agent.registerTool('foo', 'd', { type: 'object' }, () => ({
      content: [{ type: 'text', text: 'x' }],
      is_error: false,
      metadata: {},
      elapsed_ms: 0,
    }));
    expect(await agent.prompt('hi')).toBe('mock-reply');
  }, 10_000);

  it('shutdown 子命令序列化 + 子进程退出', async () => {
    const closeP = agent.close();
    // 等到子进程 exit / zhuye exit code == 0
    const exitPromise = new Promise<number | null>((resolve) => {
      child.once('exit', (code) => resolve(code));
    });
    await closeP;
    const code = await exitPromise;
    expect(code).toBe(0);
  }, 10_000);

  it('query_side_channels Op → side_channel_status 透传 + submission_closed 收尾', async () => {
    // 非 turn Op:迭代器必须收尾(v1.3 起 serve 排空 per-turn 通道后发
    // submission_closed);新事件 side_channel_status 原样透传。
    const stream = await agent.submit({ type: 'query_side_channels' } as never);
    const events: Event[] = [];
    for await (const ev of stream) events.push(ev);
    const types = events.map((e) => e.msg.type);
    expect(types).toContain('side_channel_status');
    const status = events.find((e) => e.msg.type === 'side_channel_status')!.msg as unknown as {
      channels: Array<{ id: string; status: string }>;
    };
    expect(status.channels[0]).toMatchObject({ id: 'side-00000001', status: 'running' });
    expect(types.at(-1)).toBe('submission_closed');
  }, 10_000);

  it('UnknownEventMsg 降级不抛错', async () => {
    // 每个 SDK 实例独占输入输出流,避免两个消费者争抢同一子进程的握手。
    const stdin = new PassThrough();
    const stdout = new PassThrough();
    const customAgent = await ZhuyeAgent.fromStdio(stdin, stdout, 5000);
    try {
      stdout.write(JSON.stringify({ id: '', msg: {
        type: 'session_configured', session_id: 's', model: 'mock/mock-1',
        provider: 'mock', approval_policy: 'auto', sandbox_policy: 'workspace_only',
      } }) + '\n');
      const stream = await customAgent.submit(
        { type: 'user_input', items: [{ type: 'text', text: 'probe' }] },
        { submissionId: 'unknown-probe' },
      );
      stdout.write(JSON.stringify({ id: 'unknown-probe', msg: { type: 'telemetry_event', payload: { x: 1 } } }) + '\n');
      // v1.x additive 变体:side-channel(WP9)—— 真实新事件名,SDK 原样透传。
      stdout.write(JSON.stringify({ id: 'unknown-probe', msg: {
        type: 'side_channel_started', kind: 'started', id: 'side-00000001',
        agent_name: 'a', prompt: 'p', started_at_ms: 1,
      } }) + '\n');
      stdout.write(JSON.stringify({ id: 'unknown-probe', msg: {
        type: 'turn_complete', turn_id: 't', status: 'success', usage: {},
      } }) + '\n');
      const events: Event[] = [];
      for await (const event of stream) events.push(event);
      expect(events[0].msg).toEqual({ type: 'telemetry_event', payload: { x: 1 } });
      expect(events[1].msg.type).toBe('side_channel_started');
      expect((events[1].msg as Record<string, unknown>).id).toBe('side-00000001');
      expect(events.at(-1)?.msg.type).toBe('turn_complete');
    } finally {
      await customAgent.close();
      stdout.end();
    }
  }, 10_000);

  it('tool_call_args_delta 透传到 turn 事件流(additive 变体)', async () => {
    // 协议变更是 additive 也必须同步 SDK —— 客户端要能拿到工具参数增量
    // 做渐进渲染;字段名保持 snake_case,与 Rust serde 一致。
    const events: EventMsg[] = [];
    for await (const ev of await agent.submit('foo')) {
      events.push(ev.msg);
      if (ev.msg.type === 'turn_complete') break;
    }
    const d = events.find((e) => e.type === 'tool_call_args_delta');
    expect(d).toBeDefined();
    expect(d).toMatchObject({
      type: 'tool_call_args_delta',
      call_id: 'c1',
      delta: '{"text":"pi',
    });
  }, 10_000);

  it('rescan_extensions Op → extensions_rescanned 透传(additive)', async () => {
    const stream = await agent.submit(
      { type: 'rescan_extensions', skills: true, commands: false } as never,
      { submissionId: 'rescan-probe' },
    );
    (child.stdout as Readable).push(
      JSON.stringify({ id: 'rescan-probe', msg: {
        type: 'extensions_rescanned', skills: 12, commands: false,
      } }) + '\n',
    );
    (child.stdout as Readable).push(
      JSON.stringify({ id: 'rescan-probe', msg: { type: 'submission_closed' } }) + '\n',
    );
    const events: Event[] = [];
    for await (const event of stream) events.push(event);
    expect(events[0].msg.type).toBe('extensions_rescanned');
    const evt = events[0].msg as Record<string, unknown>;
    expect(evt['skills']).toBe(12);
    expect(evt['commands']).toBe(false);
    expect(events[events.length - 1].msg.type).toBe('submission_closed');
  });


  it('工具输出增量字段透传', async () => {
    const events: EventMsg[] = [];
    for await (const ev of await agent.submit('foo')) events.push(ev.msg);
    expect(events.find((e) => e.type === 'tool_call_output_delta')).toEqual({
      type: 'tool_call_output_delta', call_id: 'c1', delta: 'preview', is_stderr: true,
    });
    expect(events.at(-1)?.type).toBe('turn_complete');
  }, 10_000);

  it('未知事件变体降级不崩', async () => {
    // 协议新增变体时,SDK 不得因不认识而抛错(按未知 type 降级)。
    (child.stdout as Readable).push(
      JSON.stringify({ id: '', msg: { type: 'some_future_event', payload: { x: 1 } } }) + '\n',
    );
    // 不抛即通过;再确认 agent 仍可用。
    expect(await agent.prompt('foo')).toBe('mock-reply');
  }, 10_000);

  it('迭代所有 turn 事件类型后正确收尾', async () => {
    const events: EventMsg[] = [];
    for await (const ev of await agent.submit('foo')) {
      events.push(ev.msg);
      if (ev.msg.type === 'turn_complete') break;
    }
    const types = events.map((e) => e.type);
    expect(types).toContain('turn_started');
    expect(types).toContain('agent_message_delta');
    expect(types[types.length - 1]).toBe('turn_complete');
  }, 10_000);

  it('nextGlobalEvent 读取全局事件(含握手事件本身)', async () => {
    // 握手 session_configured(id="")也进入全局队列(对齐 Python
    // `_global_queue` 语义:K9 e2e 依赖此行为)。
    const handshake = await agent.nextGlobalEvent({ timeoutMs: 2000 });
    expect(handshake?.id).toBe('');
    expect(handshake?.msg.type).toBe('session_configured');
    // 注入一条后续全局事件:原样可读。
    (child.stdout as Readable).push(
      JSON.stringify({ id: '', msg: { type: 'token_count', info: { total: 42 } } }) + '\n',
    );
    const ev = await agent.nextGlobalEvent({ timeoutMs: 2000 });
    expect(ev?.id).toBe('');
    expect(ev?.msg.type).toBe('token_count');
    // 全局事件不得混入 submission 的事件迭代器。
    const events: EventMsg[] = [];
    for await (const e of await agent.submit('foo')) events.push(e.msg);
    expect(events.some((m) => m.type === 'token_count')).toBe(false);
  }, 10_000);

  it('nextGlobalEvent 超时抛错(对齐 Python queue.Empty)', async () => {
    // 先取走握手事件;之后无新全局事件到达 → 超时抛错而非挂死。
    await agent.nextGlobalEvent({ timeoutMs: 2000 });
    await expect(agent.nextGlobalEvent({ timeoutMs: 100 })).rejects.toThrow('timed out');
  }, 10_000);

  it('nextGlobalEvent 在 stdout EOF 后返回 null', async () => {
    const stdin = new PassThrough();
    const stdout = new PassThrough();
    const eofAgent = await ZhuyeAgent.fromStdio(stdin, stdout, 5000);
    try {
      stdout.write(JSON.stringify({ id: '', msg: {
        type: 'session_configured', session_id: 's', model: 'mock/mock-1',
        provider: 'mock', approval_policy: 'auto', sandbox_policy: 'workspace_only',
      } }) + '\n');
      stdout.end();
      // EOF 后:队列中已有的事件仍可读,随后返回 null(对齐 Python None)。
      const handshake = await eofAgent.nextGlobalEvent({ timeoutMs: 2000 });
      expect(handshake?.msg.type).toBe('session_configured');
      expect(await eofAgent.nextGlobalEvent({ timeoutMs: 2000 })).toBeNull();
    } finally {
      await eofAgent.close();
    }
  }, 10_000);

  it('prompt 遇 turn_aborted 抛错(对齐 Python,不静默返回部分文本)', async () => {
    const stdin = new PassThrough();
    const stdout = new PassThrough();
    const abortAgent = await ZhuyeAgent.fromStdio(stdin, stdout, 5000);
    try {
      stdout.write(JSON.stringify({ id: '', msg: {
        type: 'session_configured', session_id: 's', model: 'mock/mock-1',
        provider: 'mock', approval_policy: 'auto', sandbox_policy: 'workspace_only',
      } }) + '\n');
      const pending = abortAgent.prompt('hi');
      // 从 stdin 捕获 prompt 内部生成的 submission id,向同 id 通道
      // 注入 delta + turn_aborted。
      const subId = await new Promise<string>((resolve) => {
        stdin.once('data', (c: Buffer) => resolve(JSON.parse(c.toString().trim()).id));
      });
      stdout.write(JSON.stringify({ id: subId, msg: {
        type: 'agent_message_delta', delta: 'part',
      } }) + '\n');
      stdout.write(JSON.stringify({ id: subId, msg: {
        type: 'turn_aborted', turn_id: 't', reason: { type: 'user_interrupt' },
      } }) + '\n');
      await expect(pending).rejects.toThrow('zhuye turn aborted');
    } finally {
      await abortAgent.close();
    }
  }, 10_000);
});
// ── 修复回归:prompt 去重 / 握手失败定位 / stderr 接管 ───────────────────

describe('修复回归:prompt 去重 / 握手失败定位 / stderr 接管', () => {
  it('prompt:同轮 delta 后接全量 message 时全量覆盖(去重)', async () => {
    const script = `
      process.stdout.write(JSON.stringify({
        id: '', msg: { type: 'session_configured', session_id: 's', model: 'mock/mock-1',
        provider: 'mock', approval_policy: 'auto', sandbox_policy: 'workspace_only' },
      }) + '\\n');
      const readline = require('node:readline');
      const rl = readline.createInterface({ input: process.stdin });
      rl.on('line', (line) => {
        let sub; try { sub = JSON.parse(line); } catch { return; }
        if (sub.op?.type === 'shutdown') {
          process.stdout.write(JSON.stringify({ id: sub.id, msg: { type: 'shutdown_complete' } }) + '\\n');
          process.exit(0);
        }
        if (sub.op?.type === 'user_input') {
          process.stdout.write(JSON.stringify({ id: sub.id, msg: { type: 'agent_message_delta', delta: 'delta-part' } }) + '\\n');
          // 全量消息与 delta 同轮混发(协议允许):全量是权威快照。
          process.stdout.write(JSON.stringify({ id: sub.id, msg: { type: 'agent_message', text: 'final-text' } }) + '\\n');
          process.stdout.write(JSON.stringify({ id: sub.id, msg: { type: 'turn_complete', turn_id: 't', status: 'success',
            usage: { input_tokens: 0, output_tokens: 1 } } }) + '\\n');
        }
      });
    `;
    const child = spawn(process.execPath, ['-e', script], { stdio: ['pipe', 'pipe', 'pipe'] });
    const agent = await ZhuyeAgent.fromStdio(child.stdin, child.stdout);
    try {
      // 修复前:'delta-partfinal-text'(重复计数);修复后:全量覆盖 delta 缓冲。
      expect(await agent.prompt('hi')).toBe('final-text');
    } finally {
      await agent.close();
    }
  }, 15_000);

  it('spawn 握手期子进程死亡 → 首次使用立刻抛错并携带 stderr 尾部', async () => {
    // 假二进制:忽略 serve 参数,打印真实错误到 stderr 后 exit 7
    // (模拟 serve 配置错误启动即死)。TS 的 spawn() 不等握手即返回
    // (与 Python 不同),死亡错误在首次 submit() 时经内部 ready
    // promise 冒出 —— 不再傻等满握手超时。
    const { mkdtempSync, writeFileSync, chmodSync } = await import('node:fs');
    const { tmpdir } = await import('node:os');
    const { join } = await import('node:path');
    const dir = mkdtempSync(join(tmpdir(), 'zhuye-fake-bin-'));
    const bin = join(dir, 'fake-zhuye');
    writeFileSync(bin, [
      '#!/bin/sh',
      'printf %s\\n "config file corrupted: invalid [openai] section" >&2',
      'exit 7',
    ].join('\n'));
    chmodSync(bin, 0o755);
    const start = Date.now();
    const agent = await ZhuyeAgent.spawn({ bin });
    const message = await agent.submit('hi').then(
      () => '',
      (err: Error) => err.message,
    );
    expect(message).toContain('exited during handshake (code=7');
    expect(message).toContain('config file corrupted');
    // 死亡后立刻抛错,远小于默认 30s 握手超时。
    expect(Date.now() - start).toBeLessThan(15_000);
    await agent.close().catch(() => {});
  }, 20_000);

  it('onStderr 回调接管子进程 stderr', async () => {
    // 假二进制:往 stderr 打一行负载,发握手后驻留(等 close 收杀)。
    // onStderr 回调应收到该负载 —— 即 stderr 被宿主接管而非硬编码
    // 直通宿主进程。
    const { mkdtempSync, writeFileSync, chmodSync } = await import('node:fs');
    const { tmpdir } = await import('node:os');
    const { join } = await import('node:path');
    const dir = mkdtempSync(join(tmpdir(), 'zhuye-fake-bin2-'));
    const bin = join(dir, 'fake-zhuye-ok');
    writeFileSync(bin, [
      '#!/bin/sh',
      'printf %s\\n "stderr-payload-captured" >&2',
      "printf %s\\n '{\"id\":\"\",\"msg\":{\"type\":\"session_configured\",\"session_id\":\"s\",\"model\":\"m\",\"provider\":\"p\",\"approval_policy\":\"auto\",\"sandbox_policy\":\"w\"}}'",
      'sleep 30',
    ].join('\n'));
    chmodSync(bin, 0o755);
    const captured: Buffer[] = [];
    const agent2 = await ZhuyeAgent.spawn({ bin, onStderr: (c) => captured.push(c) });
    try {
      // stderr chunk 异步到达:轮询等待,至多 5s。
      const deadline = Date.now() + 5_000;
      while (
        !captured.some((c) => c.toString('utf-8').includes('stderr-payload-captured'))
        && Date.now() < deadline
      ) {
        await new Promise((r) => setTimeout(r, 50));
      }
      expect(
        captured.some((c) => c.toString('utf-8').includes('stderr-payload-captured')),
      ).toBe(true);
    } finally {
      await agent2.close();
    }
  }, 20_000);
});
