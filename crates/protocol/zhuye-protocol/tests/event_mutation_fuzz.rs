//! 协议事件变异回归:合法样本的截断/字节替换不得导致解析 panic。
//!
//! 这不是持续运行的 libFuzzer 目标,而是把最关键的 fuzz 性质固化成
//! 每次 CI 都执行的确定性回归;真正的 fuzzing 可在同一入口继续加深变异。

use std::path::Path;

use serde_json::Value;
use zhuye_protocol::EventMsg;

fn sample_lines() -> Vec<Vec<u8>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tests/contracts/events.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读取事件契约失败:{}{error}", path.display()));
    let events = serde_json::from_str::<Value>(&raw)
        .expect("events.json 必须是合法 JSON")
        .get("events")
        .and_then(Value::as_object)
        .expect("events.json 缺少 events 对象")
        .clone();
    events
        .values()
        .map(|sample| serde_json::to_vec(sample).expect("样本序列化失败"))
        .collect()
}

fn parse(bytes: &[u8]) {
    // 解析失败是合法结果;此处只要求不 panic、不无限递归。
    let _ = serde_json::from_slice::<EventMsg>(bytes);
}

#[test]
fn truncating_every_prefix_never_panics() {
    let samples = sample_lines();
    assert!(!samples.is_empty(), "事件契约样本不能为空");
    for sample in &samples {
        for end in (0..sample.len())
            .step_by(7)
            .chain(std::iter::once(sample.len()))
        {
            parse(&sample[..end]);
        }
    }
}

#[test]
fn byte_replacements_never_panic() {
    let samples = sample_lines();
    let replacements = [0x00, 0x2c, 0x7b, 0x7d, 0xff];
    for sample in &samples {
        for index in 0..sample.len() {
            for replacement in replacements {
                let mut mutated = sample.clone();
                mutated[index] = replacement;
                parse(&mutated);
            }
        }
    }
}

#[test]
fn replacing_event_discriminant_never_panics() {
    let samples = sample_lines();
    for sample in &samples {
        let mut mutated = sample.clone();
        let needle = b"\"type\"";
        if let Some(pos) = mutated
            .windows(needle.len())
            .position(|window| window == needle)
        {
            for index in pos..(pos + needle.len()).min(mutated.len()) {
                mutated[index] = 0x00;
                parse(&mutated);
                mutated = sample.clone();
            }
        }
    }
}

/// 简易 xorshift 随机源:保证同一 seed 下 CI 可复现。
struct SeededRng(u64);

impl SeededRng {
    fn next_u64(&mut self) -> u64 {
        let mut state = self.0;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.0 = state;
        state
    }

    fn below(&mut self, upper: usize) -> usize {
        (self.next_u64() % upper as u64) as usize
    }
}

#[test]
fn seeded_multi_site_mutations_never_panic() {
    let cases = std::env::var("ZHUYE_FUZZ_CASES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(10_000);
    let seed = std::env::var("ZHUYE_FUZZ_SEED")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0x5a485945);
    let samples = sample_lines();
    assert!(!samples.is_empty(), "事件契约样本不能为空");
    let mut rng = SeededRng(seed | 1);

    for _ in 0..cases {
        let sample = &samples[rng.below(samples.len())];
        let mut mutated = sample.clone();
        let mutation_count = 1 + rng.below(4);
        for _ in 0..mutation_count {
            let index = rng.below(mutated.len());
            mutated[index] = (rng.next_u64() & 0xff) as u8;
        }
        parse(&mutated);
    }
}
