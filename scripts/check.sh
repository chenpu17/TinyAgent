#!/usr/bin/env bash
# 本地 CI 一把过：把 .github/workflows/ci.yml 里 fmt / test / code 三个 job 的
# 命令原样搬到本地跑。命令与 ci.yml 必须逐字一致——本地绿灯才等于 CI 绿灯；
# 改了 ci.yml 记得回来同步这里（参数、-D warnings、--features mock 都别漏）。
#
# 用法：
#   ./scripts/check.sh              # 跑默认全集（fmt clippy test e2e docs）
#   ./scripts/check.sh fmt docs     # 只跑指定分组，可给多个
#   ./scripts/check.sh e2e          # 只跑场景 e2e（内置 SSE mock，零密钥）
#   ./scripts/check.sh all          # 等价默认（仍不含 bindings / bench）
#   ./scripts/check.sh bindings     # 双绑定 clippy（默认不跑，见下）
#   ./scripts/check.sh bench        # 性能基准（快跑一轮，默认不跑，见下）
#
# 有意不 set -e：全部检查跑完再汇总，避免第一条红灯掩盖后面独立的问题。
# 也不并行：cargo 自身按锁串行，并行只会互相阻塞且打乱报错归属。

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

FAILED=()
PASSED=0
TOTAL_START=$SECONDS

header() {
    printf '\n\033[1;34m==> %s\033[0m\n' "$1"
}

# 先打印命令行再执行：失败时人可以直接复制那行单独重跑，不用回翻脚本。
run_check() {
    local name="$1"
    shift
    header "$name"
    printf '+ %s\n' "$*"
    local start=$SECONDS
    if "$@"; then
        printf '\033[32m[ok]\033[0m %s (%ss)\n' "$name" "$((SECONDS - start))"
        PASSED=$((PASSED + 1))
    else
        printf '\033[31m[FAIL]\033[0m %s (%ss)\n' "$name" "$((SECONDS - start))"
        FAILED+=("$name")
    fi
}

group_fmt() {
    run_check "fmt" cargo fmt --all --check
}

group_clippy() {
    # 四个配置缺一不可：feature 后的代码在其他配置里根本看不到
    # （tui / code / tui×code 交叉面），单跑一两个即漏检。
    run_check "clippy: headless" \
        cargo clippy -p tiny-agent-core -p tiny-agent --all-targets -- -D warnings
    run_check "clippy: tui" \
        cargo clippy -p tiny-agent --features tui --all-targets -- -D warnings
    run_check "clippy: code" \
        cargo clippy -p tiny-agent-core -p tiny-agent --features code --all-targets -- -D warnings
    run_check "clippy: tui,code" \
        cargo clippy -p tiny-agent --features tui,code --all-targets -- -D warnings
}

group_test() {
    # 带 mock 的命令才真正编到 lib_api.rs（集成测试的 lib 不带 cfg(test)），
    # 不带 mock 会把它编成 0 个测试照样全绿——别嫌慢省掉。
    run_check "test: default" \
        cargo test -p tiny-agent-core -p tiny-agent
    run_check "test: core mock" \
        cargo test -p tiny-agent-core --features mock
    run_check "test: tui,mock" \
        cargo test -p tiny-agent --features tui,mock
    run_check "test: code,mock" \
        cargo test -p tiny-agent-core -p tiny-agent --features code,mock
    # ci.yml 的 code job 还把 code 版编成真实二进制再跑 e2e；这里同样先 build，
    # 否则 e2e 用的是上次遗留的 target 产物，可能和当前源码对不上。
    run_check "build: code" \
        cargo build --features code
}

group_e2e() {
    # 内置 SSE 场景 mock（线程化、零密钥），走真实 CLI/HTTP 栈守四个 loop 级
    # 行为：打转护栏、并行 task fan-out、SIGINT exit 130、模型路由/批量并发。
    # 改了 loop / 取消 / 打转行为后必跑（CLAUDE.md「离线 / 视觉验证」）。
    run_check "e2e: scenarios" \
        python3 scripts/e2e_scenarios.py
}

group_docs() {
    # 三处同源契约 / 生成物 / 评测装置的漂移门，与 ci.yml 的 fmt job 同款。
    run_check "docs: run_code contract" \
        python3 scripts/sync_run_code_contract.py --check
    run_check "docs: structure tree" \
        python3 scripts/gen_structure.py --check
    run_check "docs: eval apparatus" \
        python3 scripts/check_eval_apparatus.py
}

group_bench() {
    # 性能基准（crates/core/benches/loop_bench.rs，零依赖手搓 harness）。
    # 默认不跑：数字只在本机可比、CI 不设性能门，且 --quick 一轮也要几十秒。
    # 改了 agent loop / context_breakdown / confined_to 后手动跑一轮看趋势。
    # 必须带 mock（loop 组用脚本化 Provider，不连网）；不加 --quick 则每组采样 ~1s。
    # 顺手 lint 一遍：bench 的 agent_bench 模块门在 mock 后，四个 CI clippy 配置
    # （都不带 mock）根本看不到它——不在这里补就永远没人 lint。
    run_check "clippy: bench (mock)" \
        cargo clippy -p tiny-agent-core --features mock --all-targets -- -D warnings
    run_check "bench: loop_bench" \
        cargo bench -p tiny-agent-core --features mock --bench loop_bench -- --quick
}

group_bindings() {
    # 默认不跑：需要 bindings/{python,node} 的依赖环境（maturin / napi），
    # 纯净 checkout 上跑不通。本地装好了再手动 `./scripts/check.sh bindings`。
    run_check "clippy: tiny-agent-py" \
        cargo clippy -p tiny-agent-py --all-targets -- -D warnings
    run_check "clippy: tiny-agent-py code" \
        cargo clippy -p tiny-agent-py --features code --all-targets -- -D warnings
    run_check "clippy: tiny-agent-node" \
        cargo clippy -p tiny-agent-node --all-targets -- -D warnings
    run_check "clippy: tiny-agent-node code" \
        cargo clippy -p tiny-agent-node --features code --all-targets -- -D warnings
}

usage() {
    printf '用法: %s [fmt|clippy|test|e2e|docs|bench|bindings|all]...（默认 all，不含 bench/bindings）\n' "$0"
}

CHECKS=()
for arg in "$@"; do
    case "$arg" in
        fmt|clippy|test|e2e|docs|bench|bindings) CHECKS+=("$arg") ;;
        # all 刻意不含 bench / bindings：前者没有 CI 门（数字只在本机可比），
        # 后者纯净 checkout 跑不通。要跑显式点名。
        all) CHECKS=(fmt clippy test e2e docs) ;;
        -h|--help) usage; exit 0 ;;
        *) usage; exit 2 ;;
    esac
done
if [ ${#CHECKS[@]} -eq 0 ]; then
    CHECKS=(fmt clippy test e2e docs)
fi

for group in "${CHECKS[@]}"; do
    "group_$group"
done

printf '\n\033[1m==== 汇总（%ss）: %d 通过, %d 失败 ====\033[0m\n' \
    "$((SECONDS - TOTAL_START))" "$PASSED" "${#FAILED[@]}"
if [ ${#FAILED[@]} -gt 0 ]; then
    for name in "${FAILED[@]}"; do
        printf '  \033[31mFAIL\033[0m %s\n' "$name"
    done
    exit 1
fi
printf '  \033[32m全部通过\033[0m\n'
