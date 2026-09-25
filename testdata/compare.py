# -*- coding: utf-8 -*-
"""比较 Python 参照结果与 Rust 验证结果是否一致。
用法: python compare.py <ref.json> <rust.json>"""
import json
import sys

a = json.load(open(sys.argv[1], encoding='utf-8-sig'))
b = json.load(open(sys.argv[2], encoding='utf-8-sig'))

keys = ['rows', 'cols', 'hits1', 'hits2', 'hits3', 'hits4', 'hashes']
all_ok = True
for k in keys:
    if a[k] == b[k]:
        print(f"{k}: MATCH")
    else:
        all_ok = False
        al = a[k] if not isinstance(a[k], list) else f"[len={len(a[k])}] {a[k][:10]}"
        bl = b[k] if not isinstance(b[k], list) else f"[len={len(b[k])}] {b[k][:10]}"
        print(f"{k}: DIFF\n  python={al}\n  rust  ={bl}")

print("ALL MATCH" if all_ok else "HAS DIFF")
sys.exit(0 if all_ok else 1)
