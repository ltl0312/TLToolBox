#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""TLToolBox release.yml 本地验证：语法解析 + 与仓库构建契约的一致性检查。"""
import os
import sys
import yaml

REPO = r"D:\Code\Rust\TLToolBox"
WF = os.path.join(REPO, r".github\workflows\release.yml")

errors = []

with open(WF, encoding="utf-8") as f:
    raw = f.read()

# ---- 1. 纯 YAML 语法解析（缩进/引号/结构错误在此暴露）----
try:
    doc = yaml.safe_load(raw)
    print("[PASS] YAML 语法解析成功")
except yaml.YAMLError as e:
    print("[FAIL] YAML 语法错误:\n", e)
    sys.exit(1)

# ---- 2. 结构断言 ----
assert isinstance(doc, dict), "根节点应为 mapping"
# PyYAML 按 YAML 1.1 解析，键 `on` 会变成布尔 True；GitHub 按 YAML 1.2 处理，
# 文件本身无错，这里两种键都兼容。
on = doc.get("on") or doc.get(True) or {}
push = (on or {}).get("push") or {}
tags = push.get("tags") or []
if tags != ["v*"]:
    errors.append(f"push.tags 应为 ['v*']，实际 {tags}")
if "workflow_dispatch" not in on:
    errors.append("缺少 workflow_dispatch 手动触发")
if doc.get("runs-on") is not None:
    errors.append("runs-on 应位于 job 级")

jobs = doc.get("jobs") or {}
assert "release" in jobs, "缺少 release job"
job = jobs["release"]
if job.get("runs-on") != "windows-latest":
    errors.append(f"runs-on 应为 windows-latest，实际 {job.get('runs-on')}")
if job.get("timeout-minutes", 0) < 60:
    errors.append("timeout-minutes 建议 >= 60（LTO 全量编译）")

perms = doc.get("permissions") or {}
if perms.get("contents") != "write":
    errors.append("需要 permissions.contents: write（创建 Release）")

steps = job.get("steps") or []
names = [s.get("name", "") for s in steps]
joined = "\n".join(names)

# ---- 3. 关键步骤存在性 ----
required = [
    ("Rust 工具链", "dtolnay/rust-toolchain"),
    ("MSVC", "ilammy/msvc-dev-cmd"),
    ("测试", None),
    ("编译", None),
    ("打包", None),
    ("GitHub Release", "softprops/action-gh-release"),
]
for label, action in required:
    if label not in joined and (action is None or action not in "\n".join(
            s.get("uses", "") for s in steps)):
        errors.append(f"缺少步骤: {label}")

# ---- 4. 构建命令逐字一致 ----
cmd_runs = "\n".join(s.get("run", "") for s in steps)
if "cargo test --release --all-targets" not in cmd_runs:
    errors.append("缺少命令: cargo test --release --all-targets")
if "cargo build --release" not in cmd_runs:
    errors.append("缺少命令: cargo build --release")

# ---- 5. 触发 tag 与 Release 门控一致性 ----
release_step = next((s for s in steps if s.get("uses", "").startswith("softprops/action-gh-release")), None)
if release_step is None:
    errors.append("缺少 softprops/action-gh-release 步骤")
else:
    cond = release_step.get("if", "")
    if "startsWith(github.ref, 'refs/tags/v')" not in cond:
        errors.append(f"Release 步骤缺少 Tag 门控 if，实际: {cond}")
    files = (release_step.get("with", {}) or {}).get("files", "")
    for f in ("tltoolbox-windows-x86_64.zip", "tltoolbox-windows-x86_64.zip.sha256"):
        if f not in files:
            errors.append(f"Release 资产缺少 {f}")

# ---- 6. 与仓库事实的一致性 ----
with open(os.path.join(REPO, "Cargo.toml"), encoding="utf-8") as f:
    cargo = f.read()
if "name = \"tltoolbox\"" not in cargo:
    errors.append("Cargo.toml 包名应为 tltoolbox（决定 exe 名）")
if "[[bin]]" in cargo:
    errors.append("存在自定义 [[bin]] 声明，需核对 exe 产物名是否仍为 tltoolbox.exe")
if not os.path.isfile(os.path.join(REPO, "config", "tltoolbox.toml")):
    errors.append("缺少 config/tltoolbox.toml（打包源文件）")
if os.path.exists(os.path.join(REPO, "rust-toolchain.toml")):
    errors.append("存在 rust-toolchain.toml，需与 workflow 的 stable 对齐")
# 打包文件名与 Cargo 版本无关（固定名），仅校验 exe 路径引用一致
if "target/release/tltoolbox.exe" not in cmd_runs:
    errors.append("打包步骤未引用 target/release/tltoolbox.exe")

# ---- 7. 表达式与缩进卫生 ----
if "\t" in raw:
    # 制表符只允许出现在注释文本里；逐行校验非注释行
    bad = [i + 1 for i, ln in enumerate(raw.splitlines())
           if "\t" in ln and ln.strip().startswith(("#",)) is False]
    if bad:
        errors.append(f"非注释行含制表符: 行 {bad}")

if errors:
    print("[FAIL] 共 %d 项不一致:" % len(errors))
    for e in errors:
        print("  -", e)
    sys.exit(1)

print("[PASS] 全部一致性检查通过")
print("  triggers :", tags, "+ workflow_dispatch")
print("  runs-on  : windows-latest | timeout:", job.get("timeout-minutes"))
print("  steps    :", len(steps))
print("  assets   : tltoolbox-windows-x86_64.zip + .sha256 -> GitHub Release (tag 门控)")
