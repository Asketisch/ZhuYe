//! 核心枚举栈尺寸测量(只读探针,长期保留作为内存布局回归基线)。
//!
//! EventMsg / Op 经 mpsc 通道按值搬运,栈尺寸即每次 send/recv 的
//! 拷贝成本;变大说明新载荷被不加约束地内联进变体。基线值写入
//! 断言,新增大变体时应考虑 Box 化以维持基线。

use std::mem::size_of;

use zhuye_protocol::{Event, EventMsg, Op, Submission, ToolOutput, UserInputItem};

#[test]
fn core_enum_sizes_stay_within_baseline() {
    println!("size_of::<Op>()            = {}", size_of::<Op>());
    println!("size_of::<EventMsg>()      = {}", size_of::<EventMsg>());
    println!("size_of::<Event>()         = {}", size_of::<Event>());
    println!("size_of::<Submission>()    = {}", size_of::<Submission>());
    println!(
        "size_of::<UserInputItem>() = {}",
        size_of::<UserInputItem>()
    );
    println!("size_of::<ToolOutput>()    = {}", size_of::<ToolOutput>());

    // 基线(2026-11 测量,arm64/debug)。超限说明有变体载荷膨胀,
    // 需要评估 Box 化,而不是直接放宽阈值。
    assert!(size_of::<Op>() <= 200, "Op 栈尺寸膨胀:{}", size_of::<Op>());
    assert!(
        size_of::<EventMsg>() <= 280,
        "EventMsg 栈尺寸膨胀:{}",
        size_of::<EventMsg>()
    );
    assert!(
        size_of::<Event>() <= 320,
        "Event 栈尺寸膨胀:{}",
        size_of::<Event>()
    );
}
