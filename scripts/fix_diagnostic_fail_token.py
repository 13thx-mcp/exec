#!/usr/bin/env python3
from pathlib import Path

path = Path(__file__).resolve().parents[1] / "src" / "main.rs"
text = path.read_text()
old = '''        let is_failure = lower.contains("failed")
            || lower.contains("failure")
            || lower.contains("fail:")
            || lower.contains("✖")
            || lower.contains("not ok");
'''
new = '''        let is_failure = lower.contains("failed")
            || lower.contains("failure")
            || lower == "fail"
            || lower.starts_with("fail ")
            || lower.starts_with("fail\\t")
            || lower.contains("fail:")
            || lower.contains("✖")
            || lower.contains("not ok");
'''

if new in text:
    print("diagnostic fail-token fix already applied")
    raise SystemExit(0)
if old not in text:
    raise SystemExit("expected diagnostic matcher was not found; source may have changed")
path.write_text(text.replace(old, new, 1))
print(f"patched {path}")
