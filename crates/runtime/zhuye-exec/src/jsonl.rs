//! headless `zhuye exec` stdout 流的 JSONL 事件 writer。

use std::io::Write;

// ── JsonlWriter(JSONL 写入器)─────────────────────────────────────────────

/// 流式 delta 事件的攒批阈值:单条 delta 通常几十字节,逐条 flush 会让
/// 流式回复产生数千次 write 系统调用;攒到该字节数再冲。非 delta 事件
/// (Turn / 工具边界等)立即冲出 —— 交互语义不变,且会顺带把此前攒批的
/// delta 一并带出。
const DELTA_BATCH_FLUSH_BYTES: usize = 4096;

pub struct JsonlWriter<W: Write> {
    inner: std::io::BufWriter<W>,
}

impl<W: Write> JsonlWriter<W> {
    pub fn new(w: W) -> Self {
        Self {
            inner: std::io::BufWriter::new(w),
        }
    }
    /// 写入一条事件。流式 delta 事件攒批(缓冲未达阈值时不冲),其余
    /// 事件立即 flush;调用方在排水循环结束后应调用 [`Self::flush`]
    /// 冲出尾部滞留的 delta。
    pub fn write_event(&mut self, event: &zhuye_protocol::Event) -> std::io::Result<()> {
        serde_json::to_writer(&mut self.inner, event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        self.inner.write_all(b"\n")?;
        // delta 且缓冲未满 → 攒批;否则(非 delta,或缓冲已达阈值)冲出。
        if is_stream_delta(&event.msg) && self.inner.buffer().len() < DELTA_BATCH_FLUSH_BYTES {
            return Ok(());
        }
        self.inner.flush()
    }
    /// 冲出缓冲中滞留的事件(循环结束 / 进程退出前调用)。
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
    /// 测试助手:当前 BufWriter 中未冲出的字节数(攒批滞留量)。
    #[cfg(test)]
    pub(crate) fn buffered_len(&self) -> usize {
        self.inner.buffer().len()
    }
}

/// 是否为流式增量事件(高频、单条小、允许微批延迟)。
fn is_stream_delta(msg: &zhuye_protocol::EventMsg) -> bool {
    matches!(
        msg,
        zhuye_protocol::EventMsg::AgentMessageDelta(_)
            | zhuye_protocol::EventMsg::ThinkingDelta(_)
            | zhuye_protocol::EventMsg::ToolCallOutputDelta(_)
    )
}
