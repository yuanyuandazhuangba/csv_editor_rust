# -*- coding: utf-8 -*-
"""参照脚本：用 Python 标准库验证筛选/哈希结果，与 Rust 版对照。
用法: python verify_ref.py <csv路径>"""
import csv
import hashlib
import json
import sys

path = sys.argv[1]

rows = []
with open(path, encoding='utf-8-sig', newline='') as f:
    r = csv.reader(f)
    header = next(r)
    for line in r:
        rows.append(line)
col = {name: i for i, name in enumerate(header)}


def num(s):
    t = s.strip()
    if not t:
        return None
    try:
        return float(t)
    except ValueError:
        return None


# 筛选1：交易账号 == 15270188000168848（数值相等）
hits1 = [
    i for i, row in enumerate(rows)
    if row[col['交易账号']].strip() == '15270188000168848'
]

# 筛选2：交易金额 > 100（数值比较，空值忽略）
hits2 = [
    i for i, row in enumerate(rows)
    if (n := num(row[col['交易金额']])) is not None and n > 100
]

# 筛选3：无效标签_新增 == 无效（字符串相等）
hits3 = [
    i for i, row in enumerate(rows)
    if row[col['无效标签_新增']].strip() == '无效'
]

# 筛选4：数据来源_银行名称 包含 "银行"
hits4 = [
    i for i, row in enumerate(rows)
    if '银行' in row[col['数据来源_银行名称']]
]

# 哈希：ID + 交易账号 拼接后 md5（前 50 行）
hashes = [
    hashlib.md5(
        (row[col['ID']] + row[col['交易账号']]).encode('utf-8')
    ).hexdigest()
    for row in rows[:50]
]

print(json.dumps({
    'rows': len(rows),
    'cols': len(header),
    'hits1': hits1,
    'hits2': hits2,
    'hits3': hits3,
    'hits4': hits4,
    'hashes': hashes,
}, ensure_ascii=False))
