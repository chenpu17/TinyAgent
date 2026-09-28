#!/bin/sh
# TinyAgent run_code 入门演示。
#
# 生成埋了 2^53 级大数的数据文件 → 让模型用一次 run_code 在沙箱里
# 精确统计（BigInt 全程保真）→ 写回 stats.json → 用 python3 交叉验证，
# 并展示 float64 直接求和会错成什么样。
#
# 需要的环境变量（README 有说明）：
#   TINYAGENT_DEMO_BASE_URL / TINYAGENT_DEMO_MODEL / TINYAGENT_API_KEY
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
DEMO="$ROOT/examples/run-code-demo"

if ! command -v python3 >/dev/null 2>&1; then
    echo "演示需要 python3，用于读取配置与独立核对结果。" >&2
    exit 1
fi

# ── 1. 找二进制（须带 code feature 编译）────────────────────────────────
if [ -n "${TINYAGENT_BIN:-}" ]; then
    BIN="${TINYAGENT_BIN}"
    if [ ! -x "$BIN" ]; then
        echo "TINYAGENT_BIN 不可执行：$BIN" >&2
        exit 1
    fi
elif [ -x "$ROOT/target/release/tiny-agent" ]; then
    BIN="$ROOT/target/release/tiny-agent"
elif [ -x "$ROOT/target/debug/tiny-agent" ]; then
    BIN="$ROOT/target/debug/tiny-agent"
else
    echo "未找到 tiny-agent 二进制。先构建：cargo build --release --features code" >&2
    exit 1
fi

# 在请求模型前检查实际产物，避免开关已开但二进制没有沙箱。
VERSION_JSON=$("$BIN" --version --json)
printf '%s\n' "$VERSION_JSON" | python3 -c '
import json, sys
info = json.load(sys.stdin)
if "code" not in info.get("features", []):
    sys.exit("二进制未编入 code；请使用带 code 的产物或 cargo build --release --features code")
print("── 二进制：", info["version"], ", features:", ", ".join(info["features"]))
'

# ── 2. 环境变量检查（key 只进环境变量，绝不写进任何文件）────────────────
: "${TINYAGENT_DEMO_BASE_URL:?请 export TINYAGENT_DEMO_BASE_URL（OpenAI 兼容网关地址）}"
: "${TINYAGENT_DEMO_MODEL:?请 export TINYAGENT_DEMO_MODEL（模型名）}"
: "${TINYAGENT_API_KEY:?请 export TINYAGENT_API_KEY（API key）}"

# ── 3. 独立工作区，保留之前的结果 ──────────────────────────────────────
mkdir -p "$DEMO/work"
WORK=$(mktemp -d "$DEMO/work/run.XXXXXX")
echo "── 本次工作区：$WORK"

# 297 个小整数 + 3 个超出 Number 安全范围（±2^53）的大数。
# float64 能精确表达到 2^53=9007199254740992；下面的后两个都不行，
# 三数之和更是必然失真。
seq 1 297 > "$WORK/data.txt"
cat >> "$WORK/data.txt" <<'EOF'
9007199254740991
9007199254740993
4611686018427387904
EOF

python3 - "$DEMO/config.example.json" "$WORK" <<'PY'
import json, os, sys
from pathlib import Path
cfg = json.loads(Path(sys.argv[1]).read_text())
cfg.update(base_url=os.environ["TINYAGENT_DEMO_BASE_URL"],
           model=os.environ["TINYAGENT_DEMO_MODEL"], workdir=sys.argv[2])
if "TINYAGENT_DEMO_MAX_TOKENS" in os.environ:
    try:
        cap = int(os.environ["TINYAGENT_DEMO_MAX_TOKENS"])
    except ValueError:
        sys.exit("TINYAGENT_DEMO_MAX_TOKENS 必须是正整数")
    if cap <= 0:
        sys.exit("TINYAGENT_DEMO_MAX_TOKENS 必须是正整数")
    cfg["max_tokens"] = cap
Path(sys.argv[2], "config.json").write_text(
    json.dumps(cfg, ensure_ascii=False, indent=2) + "\n")
PY

# 任务措辞要点：所有数值字段一律「精确十进制字符串」——avg 的量级
# ~1.5e16 本身就超 float64 精度，任何数值型表示都会静默失真，字符串
# 是唯一无损形态（与 run-code 技能的数值纪律一致）。
TASK='data.txt 每行一个整数。用一次 run_code 调用完成统计：读入全部整数（其中有超出 Number 安全范围的值，求和必须全程用 BigInt），统计总行数、精确总和、平均值（四舍五入保留 3 位小数）、最大值、最小值；把结果作为 JSON 对象写到 stats.json，键为 lines/total/avg/max/min——除 lines 外全部用精确十进制字符串表示（avg 形如 xxx.xxx，也用字符串）。最后在回复里报告 total。'

echo "── 数据文件（尾部 5 行）："
tail -5 "$WORK/data.txt"
echo
echo "── 运行 agent（首次 run_code 有 wasmtime 冷编译，稍候）："
"${BIN}" --config "$WORK/config.json" --session none \
         --task "${TASK}" --output "$WORK/result.json"

echo
echo "── 沙箱写回的 stats.json："
cat "$WORK/stats.json"
echo

# ── 4. 交叉验证（任意精度 int；不一致即失败）──────────────────────────
python3 - "$WORK" <<'PY'
import json, sys
w = sys.argv[1]
nums = [int(l) for l in open(f"{w}/data.txt") if l.strip()]
stats = json.load(open(f"{w}/stats.json"))
total = sum(nums)
# 与 guest 相同口径：先放大 1000 倍取整数再四舍五入（round half up），
# 拼回 xxx.xxx 字符串——全程不经过 float。
scaled = (total * 1000 * 2 + len(nums)) // (2 * len(nums))
avg = f"{scaled // 1000}.{scaled % 1000:03d}"
checks = [
    ("lines", stats.get("lines"), len(nums)),
    ("total", stats.get("total"), str(total)),
    ("avg",   stats.get("avg"),   avg),
    ("max",   stats.get("max"),   str(max(nums))),
    ("min",   stats.get("min"),   str(min(nums))),
]
print("── 交叉验证（python3 任意精度，逐字段）:")
allok = set(stats) == {name for name, _, _ in checks}
for name, got, want in checks:
    ok = type(got) is type(want) and got == want
    allok = allok and ok
    print(f"   {name:5s} guest={got!r:>24}  期望={want!r:>24}  {'✓' if ok else '✗'}")
wrong = sum(float(n) for n in nums)
print(f"   float64 直接求和 = {wrong:.0f}   ← JS Number 会得到的（错误）总数")
print("   结论：", "全部精确一致 ✓" if allok else "存在不一致 ✗")

print("── 审计面：result.json 里本轮工具调用")
d = json.load(open(f"{w}/result.json"))
for c in d['tool_calls']:
    print('  -', c['name'], '| is_error:', c['is_error'])
print('  stop:', d['stop_reason'], '| turns:', d['turns'],
      '| tokens:', d['input_tokens'], '/', d['output_tokens'])
used_code = any(c['name'] == 'run_code' and not c['is_error']
                for c in d['tool_calls'])
if not used_code:
    print("   未发现成功的 run_code 调用，本次未完成沙箱演示。")
sys.exit(0 if allok and used_code and d['stop_reason'] == 'end_turn' else 1)
PY

echo
echo "演示工作区保留在 ${WORK}（含 data.txt / stats.json / result.json），可翻看。"
