//! M0 技术探针 — `docs/code_llm_design.md` §7「前置（M0，评测与拓扑的地基）」
//! 与 §9 的 M0 行。在真实类型上原型验证三件事，M3 落 `llm` 原语时直接沿用：
//!
//! 1. **按请求 model 分流**（A-L9）：同一 Provider 实例按 `ChatRequest.model`
//!    服务多套脚本；服务不了的 model 诚实报错 —— LD-16 同实例/自定义形态。
//! 2. **reserve/settle 账本**（A-L2b/2c）：预占恰好一次、结算恰好一次、未启动
//!    全退、未知用量保留估算、actual 超额可审计、拒绝不改账。
//! 3. **共享 Provider cell**（A-L9 clone 钉子）：`Arc<OnceLock<Arc<dyn Provider>>>`
//!    晚填充对全体 clone 可见；Provider panic 不破坏 cell；串行（互斥锁）
//!    Provider 照常工作（吞吐下降被接受）。
//!
//! 非流式消费属类型层事实：每条探针都以增量事件回调调用 `chat()` 并只消费返回
//! 的完整 `ChatResponse`。并行 e2e（A-L4 的时间区间重叠证明）按既有纪律留给
//! M2 的 ThreadingHTTPServer 场景，不在进程内冒充。

use crate::provider::mock::{mk_usage, resp_text, MockProvider};
use crate::provider::{Block, ChatEvent, ChatRequest, ChatResponse, Message, Provider, Role};
use crate::tools::BudgetGate;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, OnceLock};

/// One single-shot call through `provider`, mirroring how the sandbox `llm`
/// primitive (M3) will consume it: a one-off request (no history reuse), a
/// no-op delta sink, the returned `ChatResponse` as the payload.
fn call(provider: &dyn Provider, model: &str) -> anyhow::Result<ChatResponse> {
    let cancel = Arc::new(AtomicBool::new(false));
    let msgs = vec![Message {
        id: None, // 稳定 id 由 ingest 点分配，构造时留 None
        role: Role::User,
        content: vec![Block::Text("probe".into())],
    }];
    let req = ChatRequest {
        system: None,
        messages: &msgs,
        tools: &[],
        model,
        max_tokens: 64,
        per_request_timeout: None,
        cancel,
        cache: false,
        reasoning_effort: None,
        temperature: None,
    };
    let mut deltas = 0usize;
    let resp = provider.chat(&req, &mut |_e: ChatEvent| deltas += 1)?;
    // 文本响应必须既发增量又返回完整块 —— 单发消费两条通道都健康。
    assert!(deltas > 0, "text response must stream at least one delta");
    Ok(resp)
}

/// `Result::unwrap` needs `Debug` on the payload; `ChatResponse` (and the
/// `Arc<dyn Provider>` inside `OnceLock::set` errors) don't implement it.
fn unwrap_resp(r: anyhow::Result<ChatResponse>) -> ChatResponse {
    r.unwrap_or_else(|e| panic!("call failed: {e:#}"))
}

fn used(gate: &BudgetGate) -> u64 {
    gate.used.load(Ordering::Relaxed)
}

// ── A-L9：按请求 model 分流（同一实例多模型 + 诚实报错） ──────────────

#[test]
fn al9_same_instance_routes_by_request_model() {
    let (p, state) = MockProvider::routed(vec![
        ("outer", vec![resp_text("outer says hi", mk_usage(10, 5))]),
        ("inner", vec![resp_text("inner verdict", mk_usage(3, 2))]),
    ]);
    let a = unwrap_resp(call(&p, "outer"));
    let b = unwrap_resp(call(&p, "inner"));
    assert!(matches!(a.blocks.as_slice(), [Block::Text(t)] if t.as_str() == "outer says hi"));
    assert!(matches!(b.blocks.as_slice(), [Block::Text(t)] if t.as_str() == "inner verdict"));
    // 每个请求各自携带 model 字段（ChatRequest.model 是按请求的，非实例级）
    let caps = state.captured();
    assert_eq!(
        caps.iter().map(|c| c.model.as_str()).collect::<Vec<_>>(),
        ["outer", "inner"]
    );
}

#[test]
fn al9_unknown_model_is_honest_error() {
    // 自定义形态的 Provider：只会服务 outer —— 内层 model 请求必须诚实报错，
    // 而不是静默空回复或错误归属（错误路径也是账单面/审计面）。
    let (p, state) = MockProvider::routed(vec![("outer", vec![resp_text("ok", None)])]);
    let err = call(&p, "inner")
        .err()
        .expect("inner model must fail honestly");
    assert!(
        err.to_string()
            .contains("no script route for model `inner`"),
        "got: {err}"
    );
    // 失败尝试仍被 capture：归因不因失败而丢失。
    assert_eq!(state.calls(), 1);
    assert_eq!(state.captured()[0].model, "inner");
}

// ── A-L2b/2c：reserve/settle 账本矩阵 ────────────────────────────────

#[test]
fn al2b_missing_usage_keeps_reserve_totals_unfaked() {
    // A-L2b 钉子：mock 返回无 usage → 预算保留 reserve、已知量单列、totals 不伪造。
    // M3 的 LlmTool 流程在此原型化为：reserve → chat → usage 为 None 时不以
    // actual 结算（未知不伪造），guard 自然 Drop 保留估算。
    let (p, _state) = MockProvider::routed(vec![("inner", vec![resp_text("verdict", None)])]);
    let gate = BudgetGate::new(1000);
    let guard = gate.reserve(500).expect("500 fits cap 1000");
    let resp = unwrap_resp(call(&p, "inner"));
    assert!(resp.usage.is_none());
    drop(guard); // 未知用量：保留估算
    assert_eq!(used(&gate), 500);
    // totals 侧只折 Some(usage)：未知不进已知合计。
    let known_totals: u64 = resp.usage.map(|u| u.total()).unwrap_or(0);
    assert_eq!(known_totals, 0);
}

#[test]
fn al2c_settle_below_reserved_refunds_delta() {
    let gate = BudgetGate::new(1000);
    let s = gate.reserve(500).unwrap().settle(120);
    assert_eq!((s.reserved, s.actual, s.over_reserve), (500, 120, false));
    assert_eq!(used(&gate), 120);
    assert_eq!(gate.tokens_left(), 880);
}

#[test]
fn al2c_settle_above_reserved_adds_and_flags() {
    let gate = BudgetGate::new(1000);
    let s = gate.reserve(100).unwrap().settle(300);
    assert!(s.over_reserve, "超额必须可审计");
    assert_eq!(used(&gate), 300); // 100 预占 + 200 净补
}

#[test]
fn al2c_drop_without_settle_retains_full_reservation() {
    let gate = BudgetGate::new(1000);
    drop(gate.reserve(500).unwrap());
    assert_eq!(used(&gate), 500, "未知用量路径：宁可过持，不可欠持");
}

#[test]
fn al2c_panic_unwind_retains_reservation_exactly_once() {
    let gate = BudgetGate::new(1000);
    let guard = gate.reserve(400).unwrap();
    let r = catch_unwind(AssertUnwindSafe(|| {
        let _held = guard; // moved in：unwind 时 Drop → 保留估算
        panic!("provider exploded mid-flight");
    }));
    assert!(r.is_err());
    assert_eq!(used(&gate), 400, "panic 路径不退款");
    // 恰好一次的账本一致性：剩余额度既不是 0（错误退款）也不是 800（重复计）。
    assert!(
        gate.reserve(400).is_some(),
        "panic 路径不得吃掉或重复占用额度"
    );
}

#[test]
fn al2c_release_unstarted_refunds_all() {
    let gate = BudgetGate::new(1000);
    let s = gate.reserve(600).unwrap().release();
    assert_eq!((s.reserved, s.actual, s.over_reserve), (600, 0, false));
    assert_eq!(used(&gate), 0);
    assert!(gate.reserve(600).is_some(), "未启动释放后额度完整回池");
}

#[test]
fn al2c_reserve_reject_charges_nothing() {
    let gate = BudgetGate::new(1000);
    assert!(gate.reserve(800).is_some());
    assert!(
        gate.reserve(300).is_none(),
        "800+300 > cap：软预算拒纳（非 halt）"
    );
    assert_eq!(used(&gate), 800, "拒绝不改账");
}

#[test]
fn m0_concurrent_reservation_stress() {
    // 16 线程并发各预占 100、barrier 确保全部预占尝试完成后才结算 ——
    // 不变量与调度无关：恰好 10 个成功（100×10 = cap 1000），各自 settle(40)
    // 后账面 400。若预占与 cap 检查不是同一步原子（check-then-add），并发下
    // 可能有第 11 个成功；若结算重复退款，账面会偏离 400。
    let gate = BudgetGate::new(1000);
    let barrier = Barrier::new(16);
    let results = std::thread::scope(|s| {
        let hs: Vec<_> = (0..16)
            .map(|_| {
                s.spawn(|| {
                    let g = gate.reserve(100);
                    barrier.wait();
                    g.map(|g| g.settle(40))
                })
            })
            .collect();
        hs.into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    let wins = results.iter().filter(|r| r.is_some()).count();
    assert_eq!(wins, 10, "在飞预占总和不得越过 cap");
    assert_eq!(used(&gate), 400);
}

// ── A-L9：共享 Provider cell（晚填充 / clone 身份 / panic / 串行） ────

#[test]
fn al9_shared_cell_clone_identity_and_late_fill() {
    let cell: Arc<OnceLock<Arc<dyn Provider>>> = Arc::new(OnceLock::new());
    let clone = cell.clone();
    assert!(
        cell.get().is_none(),
        "晚填充前 get 为 None —— LlmTool 的明确错误前置，不悬挂"
    );
    let (p, state) = MockProvider::routed(vec![("outer", vec![resp_text("late", None)])]);
    assert!(cell.set(Arc::new(p)).is_ok());
    assert!(
        clone.set(Arc::new(MockProvider::new(vec![]).0)).is_err(),
        "二次填充被拒"
    );
    let (a, b) = (cell.get().unwrap(), clone.get().unwrap());
    assert!(Arc::ptr_eq(a, b), "clone 与本体共享同一实例（LD-16 钉子）");
    // 经 clone 派发的调用与本体同路：晚填充对全体 clone 可见。
    let resp = unwrap_resp(call(b.as_ref(), "outer"));
    assert!(matches!(resp.blocks.as_slice(), [Block::Text(t)] if t.as_str() == "late"));
    assert_eq!(state.calls(), 1);
}

#[test]
fn al9_shared_cell_concurrent_fill_single_winner() {
    let cell: Arc<OnceLock<Arc<dyn Provider>>> = Arc::new(OnceLock::new());
    let winners = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                let (p, _) = MockProvider::new(vec![]);
                if cell.set(Arc::new(p)).is_ok() {
                    winners.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
    assert_eq!(winners.load(Ordering::Relaxed), 1, "并发填充恰有一个赢家");
    let first = cell.get().unwrap();
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| assert!(Arc::ptr_eq(cell.get().unwrap(), first)));
        }
    });
}

/// A provider that always panics — the LD-17 "panic mid-flight" ledger path.
struct Panicky;
impl Provider for Panicky {
    fn chat(
        &self,
        _req: &ChatRequest,
        _on_event: &mut dyn FnMut(ChatEvent),
    ) -> anyhow::Result<ChatResponse> {
        panic!("provider exploded");
    }
}

#[test]
fn al9_provider_panic_keeps_ledger_and_cell_usable() {
    let cell: Arc<OnceLock<Arc<dyn Provider>>> = Arc::new(OnceLock::new());
    assert!(cell.set(Arc::new(Panicky)).is_ok());
    let gate = BudgetGate::new(1000);
    let guard = gate.reserve(300).unwrap();
    let r = catch_unwind(AssertUnwindSafe(|| {
        let _held = guard; // 已派发的 ReserveGuard：unwind Drop 保留估算
        call(cell.get().unwrap().as_ref(), "outer")
    }));
    assert!(r.is_err());
    assert_eq!(used(&gate), 300, "已派发未结算：保留估算");
    // 未派发全退：另一笔没发出的预占可完整释放。
    let unstarted = gate.reserve(200).unwrap().release();
    assert_eq!(unstarted.actual, 0);
    assert_eq!(used(&gate), 300);
    // cell 本身不被 panic 破坏：仍解析到同一实例，再次调用仍诚实失败。
    let again = catch_unwind(AssertUnwindSafe(|| {
        call(cell.get().unwrap().as_ref(), "outer")
    }));
    assert!(again.is_err());
}

/// A mutex-serializing provider — the "串行 Provider（互斥锁实现）照常工作，
/// 吞吐下降被接受" row of A-L9.
struct Serial {
    inner: Mutex<MockProvider>,
    cur: AtomicUsize,
    max: AtomicUsize,
}
impl Provider for Serial {
    fn chat(
        &self,
        req: &ChatRequest,
        on_event: &mut dyn FnMut(ChatEvent),
    ) -> anyhow::Result<ChatResponse> {
        let guard = self.inner.lock().unwrap();
        let cur = self.cur.fetch_add(1, Ordering::Relaxed) + 1;
        self.max.fetch_max(cur, Ordering::Relaxed);
        let r = guard.chat(req, on_event);
        self.cur.fetch_sub(1, Ordering::Relaxed);
        r
    }
}

#[test]
fn al9_serial_provider_through_shared_clones() {
    let (mock, state) = MockProvider::new((0..8).map(|_| resp_text("serial ok", None)).collect());
    let serial = Arc::new(Serial {
        inner: Mutex::new(mock),
        cur: AtomicUsize::new(0),
        max: AtomicUsize::new(0),
    });
    let cell: Arc<OnceLock<Arc<dyn Provider>>> = Arc::new(OnceLock::new());
    assert!(cell.set(serial.clone()).is_ok());
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                let resp = unwrap_resp(call(cell.get().unwrap().as_ref(), "any"));
                assert!(
                    matches!(resp.blocks.as_slice(), [Block::Text(t)] if t.as_str() == "serial ok")
                );
            });
        }
    });
    assert_eq!(state.calls(), 8);
    assert_eq!(
        serial.max.load(Ordering::Relaxed),
        1,
        "串行 Provider：同一时刻至多 1 路在飞"
    );
}
