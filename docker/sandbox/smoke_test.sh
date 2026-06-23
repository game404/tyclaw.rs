#!/usr/bin/env bash
#
# 沙盒镜像依赖预装烟雾测试（Skill Dependency Preinstall, R12 / 任务 16.2）
#
# 验证目标：
#   1. requirements.txt 已声明 matplotlib 与 openpyxl（镜像构建时即烘焙进 site-packages）。
#   2. 构建后的镜像中 matplotlib / openpyxl 可直接 import。
#   3. 首次运行不触发运行时 pip install —— 通过在 `--network none`（无网络）
#      下执行 import 来强证明：若依赖未预装而需运行时 pip install，无网络环境必然失败；
#      import 成功即证明依赖来自镜像预装层，未触发 pip install。
#   4. 进一步断言依赖模块路径落在镜像系统级 site-packages（/usr/local 或 /opt），
#      而非运行时 workspace pip 缓存（/workspace/.cache）。
#
# 设计为单次烟雾测试（非属性测试）。无网络/无 docker 时优雅降级：
# 仅运行 requirements.txt 静态校验（始终可跑），docker 相关步骤被跳过。
#
# 用法：
#   bash docker/sandbox/smoke_test.sh            # 自动探测 docker，缺失则仅做静态校验
#   IMAGE=tyclaw-sandbox:latest bash docker/sandbox/smoke_test.sh
#   BUILD=1 bash docker/sandbox/smoke_test.sh    # 镜像缺失时允许构建（构建较慢/较重）
#
# 退出码：0 = 通过（含优雅降级）；非 0 = 失败。

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REQUIREMENTS_FILE="$SCRIPT_DIR/requirements.txt"
IMAGE="${IMAGE:-tyclaw-sandbox:latest}"
BUILD="${BUILD:-0}"

# 需预装并验证的依赖（包名 -> python import 名）
PKGS=("matplotlib" "openpyxl")

pass() { echo "[ok] $*"; }
fail() { echo "[FAIL] $*" >&2; exit 1; }
skip() { echo "[skip] $*"; }

# ---------------------------------------------------------------------------
# 步骤 1：requirements.txt 静态校验（始终运行，不依赖 docker）
# ---------------------------------------------------------------------------
echo "=== Smoke test: 沙盒镜像依赖预装 (R12) ==="

[ -f "$REQUIREMENTS_FILE" ] || fail "未找到 requirements.txt: $REQUIREMENTS_FILE"

for pkg in "${PKGS[@]}"; do
    # 行首匹配包名（允许版本约束如 ==/>=/[extras]），忽略注释行
    if grep -Eiq "^[[:space:]]*${pkg}([[:space:]]|==|>=|<=|~=|!=|\[|$)" "$REQUIREMENTS_FILE"; then
        pass "requirements.txt 声明了 $pkg"
    else
        fail "requirements.txt 缺少 $pkg —— 镜像不会预装该依赖"
    fi
done

# ---------------------------------------------------------------------------
# 步骤 2：docker 探测（缺失则优雅降级）
# ---------------------------------------------------------------------------
if ! command -v docker >/dev/null 2>&1; then
    skip "未检测到 docker，跳过镜像构建与 import 验证（已完成 requirements.txt 静态校验）"
    echo "=== Smoke test 通过（降级模式：仅静态校验）==="
    exit 0
fi

if ! docker info >/dev/null 2>&1; then
    skip "docker 守护进程不可用，跳过镜像 import 验证（已完成 requirements.txt 静态校验）"
    echo "=== Smoke test 通过（降级模式：仅静态校验）==="
    exit 0
fi

# ---------------------------------------------------------------------------
# 步骤 3：确保镜像存在（按需构建，受 BUILD 开关控制）
# ---------------------------------------------------------------------------
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    if [ "$BUILD" = "1" ]; then
        echo "[build] 镜像 $IMAGE 不存在，开始构建（可能较慢）..."
        docker build -t "$IMAGE" "$SCRIPT_DIR"
        pass "镜像构建完成: $IMAGE"
    else
        skip "镜像 $IMAGE 不存在且未设置 BUILD=1，跳过 import 验证"
        echo "    提示：先执行 './tyc build-docker' 或 'BUILD=1 bash docker/sandbox/smoke_test.sh'"
        echo "=== Smoke test 通过（降级模式：仅静态校验）==="
        exit 0
    fi
fi

# ---------------------------------------------------------------------------
# 步骤 4：无网络下 import 验证（证明依赖来自预装，未触发 pip install）
# ---------------------------------------------------------------------------
# Python 探针：import 依赖并断言其路径位于系统级 site-packages（非 workspace pip 缓存）。
PY_PROBE='
import sys, importlib
mods = ["matplotlib", "openpyxl"]
for name in mods:
    m = importlib.import_module(name)
    path = getattr(m, "__file__", "") or ""
    # 运行时 pip install 会落到 /workspace/.cache 或用户级 ~/.local；预装在系统级路径。
    if "/workspace/" in path or "/.local/" in path:
        print("FAIL %s 来自运行时安装路径: %s" % (name, path))
        sys.exit(2)
    print("ok %s -> %s" % (name, path))
print("ALL_IMPORTS_OK")
'

echo "[run] 在 --network none（无网络）下验证 import ..."
# --network none：若依赖未预装而需 pip install，无网络必然失败 -> import 失败。
# --entrypoint python：绕过 Xvfb/sleep 入口，直接执行探针。
if OUT=$(docker run --rm --network none --entrypoint python "$IMAGE" -c "$PY_PROBE" 2>&1); then
    echo "$OUT"
    if echo "$OUT" | grep -q "ALL_IMPORTS_OK"; then
        pass "matplotlib / openpyxl 在无网络环境下成功 import（首次运行不触发 pip install）"
    else
        fail "import 探针输出异常：\n$OUT"
    fi
else
    echo "$OUT" >&2
    echo "    提示：镜像可能为旧版本（在 requirements.txt 新增依赖前构建）。" >&2
    echo "    请重建镜像后重试： ./tyc build-docker" >&2
    fail "无网络环境下 import 失败 —— 依赖未预装（会触发运行时 pip install）"
fi

echo "=== Smoke test 全部通过 ==="
