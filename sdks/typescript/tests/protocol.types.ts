// 这些断言由 tsc 检查,确保公开回合设置与 wire 契约一致。
import type { AbortReason, PlanApprovalChoice, ReasoningEffort, ReviewDecision, ThreadSettings, ToolCallOutputDelta, TurnComplete } from '../src/protocol.js';

type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends
  (<T>() => T extends B ? 1 : 2) ? true : false;
type Assert<T extends true> = T;

export type TurnStatusCheck = Assert<Equal<
  TurnComplete['status'], 'success' | 'max_iterations' | 'stopped' | 'token_budget_exceeded'
>>;
export type AbortReasonCheck = Assert<Equal<AbortReason,
  { type: 'user_interrupt' } | { type: 'shutdown' } | { type: 'error'; code: string; message: string }
>>;
export type PlanChoiceCheck = Assert<Equal<PlanApprovalChoice, 'auto_mode' | 'manual_approve' | 'revise'>>;
export type ReviewDecisionCheck = Assert<Equal<ReviewDecision,
  'approve' | 'approve_for_session' | { deny: { reason: string } }
>>;
export type EffortCheck = Assert<Equal<ReasoningEffort, 'low' | 'medium' | 'high' | 'max' | 'ultra'>>;
export type ToolDeltaCheck = Assert<Equal<ToolCallOutputDelta,
  { type: 'tool_call_output_delta'; call_id: string; delta: string; is_stderr: boolean }
>>;
export type ApprovalPolicyCheck = Assert<Equal<
  ThreadSettings['approval_policy'], 'auto' | 'prompt' | 'deny' | null | undefined
>>;
export type SandboxPolicyCheck = Assert<Equal<
  ThreadSettings['sandbox_policy'], 'workspace_only' | 'os_sandbox' | 'full_access' | null | undefined
>>;
export type ToolConcurrencyCheck = Assert<Equal<
  ThreadSettings['max_tool_concurrency'], number | null | undefined
>>;
