#!/bin/sh
# TinyAgent run_code 一分钟演示。
#
# 生成埋了 2^53 级大数的数据文件 → 让模型用一次 run_code 在沙箱里
# 精确统计（BigInt 全程保真）→ 写回 stats.json → 用 python3 交叉验证，
# 并展示 float64 直接求和会错成什么样。
#
# 需要的环境变量（README 有说明）：
#   TINYAGENT_DEMO_BASE_URL / TINYAGENT_DEMO_MODEL / TINYAGENT_API_KEY
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
WORK="$ROOT/examples/run-code-demo/work"

# ── 1. 找二进制（须带 code feature 编译）────────────────────────────────
if [ -n "${TINYAGENT_BIN:-}" ] && [ -x "${TINYAGENT_BIN}" ]; then
    BIN="${TINYAGENT_BIN}"
elif [ -x "$ROOT/target/release/tiny-agent" ]; then
    BIN="$ROOT/target/release/tiny-agent"
elif [ -x "$ROOT/target/debug/tiny-agent" ]; then
    BIN="$ROOT/target/debug/tiny-agent"
else
    echo "未找到 tiny-agent 二进制。先构建：cargo build --release --features code" >&2
    exit 1
fi

# ── 2. 环境变量检查（key 只进环境变量，绝不写进任何文件）────────────────
: "${TINYAGENT_DEMO_BASE_URL:?请 export TINYAGENT_DEMO_BASE_URL（OpenAI 兼容网关地址）}"
: "${TINYAGENT_DEMO_MODEL:?请 export TINYAGENT_DEMO_MODEL（模型名）}"
: "${TINYAGENT_API_KEY:?请 export TINYAGENT_API_KEY（API key）}"

# ── 3. 干净的演示工作区 + 数据文件 ──────────────────────────────────────
rm -rf "$WORK"
mkdir -p "$WORK"

# 297 个小整数 + 3 个超出 Number 安全范围（±2^53）的大数。
# float64 能精确表达到 2^53=9007199254740992；下面的后两个都不行，
# 三数之和更是必然失真。
seq 1 297 > "$WORK/data.txt"
cat >> "$WORK/data.txt" <<'EOF'
9007199254740991
9007199254740993
4611686018427387904
EOF

cat > "$WORK/config.json" <<EOF
{
  "provider": "openai",
  "base_url": "${TINYAGENT_DEMO_BASE_URL}",
  "api_key_env": "TINYAGENT_API_KEY",
  "model": "${TINYAGENT_DEMO_MODEL}",
  "max_tokens": 4096,
  "workdir": "${WORK}",
  "tools": ["shell", "read_file", "write_file"],
  "restrict_to_workdir": true,
  "limits": { "max_turns": 10, "timeout_secs": 300 },
  "code": { "enabled": true }
}
EOF

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

# ── 4. 交叉验证（有 python3 才做；任意精度 int 是天然裁判）─────────────
if command -v python3 >/dev/null 2>&1 && [ -f "$WORK/stats.json" ]; then
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
    ("total", str(stats.get("total")), str(total)),
    ("avg",   str(stats.get("avg")),   avg),
    ("max",   str(stats.get("max")),   str(max(nums))),
    ("min",   str(stats.get("min")),   str(min(nums))),
]
print("── 交叉验证（python3 任意精度，逐字段）:")
allok = True
for name, got, want in checks:
    ok = str(got) == str(want)
    allok = allok and ok
    print(f"   {name:5s} guest={got!r:>24}  期望={want!r:>24}  {'✓' if ok else '✗'}")
wrong = sum(float(n) for n in nums)
print(f"   float64 直接求和 = {wrong:.0f}   ← JS Number 会得到的（错误）总数")
print("   结论：", "全部精确一致 ✓" if allok else "存在不一致 ✗")
PY
else
    echo "（未找到 python3 或 stats.json，跳过交叉验证）"
fi

echo
echo "── 审计面：result.json 里本轮工具调用"
python3 -c "
import json
d = json.load(open('$WORK/result.json'))
for c in d['tool_calls']:
    print('  -', c['name'], '| is_error:', c['is_error'])
print('  stop:', d['stop_reason'], '| turns:', d['turns'],
      '| tokens:', d['input_tokens'], '/', d['output_tokens'])
" 2>/dev/null || sed -n '1,20p' "$WORK/result.json"

echo
echo "演示工作区保留在 ${WORK}（含 data.txt / stats.json / result.json），可翻看。"
