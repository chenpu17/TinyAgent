#!/usr/bin/env python3
"""按方案 §4.3 准备 T2 重跑批次：装置失效修后，重跑受影响的一对。

原批次 g1-20260924-r1 的 T2 两臂因 Seatbelt 挡 MCP 死于 no_result——
原记录保留为 invalid，新批次 g1-20260924-r1-t2retry 只含重跑对
（t2-B-retry1 / t2-A-retry1，新 run_id——claim_run_slot 拒绝覆盖旧记录）。
配方字段与原批次同源（configs 内容除 workdir 外逐字相同、inventory、
fingerprint、价表、停止值），config_hashes 按新 run_id 重算。
"""
import hashlib
import json
import shutil
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
src = ROOT / 'artifacts' / 'code_value' / 'g1-20260924-r1'
dst = ROOT / 'artifacts' / 'code_value' / 'g1-20260924-r1-t2retry'

if dst.exists():
    sys.exit(f'{dst} 已存在，拒绝覆盖')
dst.mkdir(parents=True)

m = json.load(open(src / 'manifest.json', encoding='utf-8'))
m['run_schedule'] = [
    {"order": 0, "run_id": "t2-B-retry1", "task": "t2", "arm": "B"},
    {"order": 1, "run_id": "t2-A-retry1", "task": "t2", "arm": "A"},
]
m['batch_id'] = 'g1-20260924-r1-t2retry'
m['retry_of'] = ('g1-20260924-r1（T2 两臂装置失效 no_result；方案 §4.3 '
                 '修后重跑受影响一对，消耗诊断储备）')

shutil.copytree(src / 'configs', dst / 'configs')
for name in ('inventory_analysis.json', 'inventory_spec.json',
             'inventory.json', 'fingerprint.json'):
    shutil.copy(src / name, dst / name)

for s in m['run_schedule']:
    old = json.load(open(dst / 'configs' / f"{s['task']}-{s['arm']}.json",
                         encoding='utf-8'))
    old['workdir'] = str(dst / 'runs' / s['run_id'] / 'wd')
    (dst / 'configs' / f"{s['run_id']}.json").write_text(
        json.dumps(old, ensure_ascii=False, indent=1), encoding='utf-8')
    (dst / 'configs' / f"{s['task']}-{s['arm']}.json").unlink()

m['config_hashes'] = {
    s['run_id']: hashlib.sha256(json.dumps(
        json.load(open(dst / 'configs' / f"{s['run_id']}.json",
                       encoding='utf-8')),
        sort_keys=True, ensure_ascii=False).encode()).hexdigest()
    for s in m['run_schedule']}
m['final_frozen_at'] = time.strftime('%Y-%m-%d %H:%M:%S')
(dst / 'manifest.json').write_text(
    json.dumps(m, ensure_ascii=False, indent=1), encoding='utf-8')
print('retry batch prepared:', dst)
print('schedule:', [s['run_id'] for s in m['run_schedule']])
print('cost_stop:', m['cost_stop'], m['cost_currency'])
