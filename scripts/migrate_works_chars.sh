#!/usr/bin/env bash
# migrate_works_chars.sh —— 把含 `/ \ : + =` 的历史 works 目录迁移到清洗后的单层 leaf。
#
# 与 control 的 filesystem_workspace_leaf() 完全一一对应：
#   leaf = workspace_key 中所有 '/' '\' ':' '+' '=' 替换为 '_'
#   bucket = md5(原始语义 key)[0:2]
#
# 处理两种历史形态：
#   1. 嵌套（slash 拆开）  : works/{bucket}/{a}/{b}/  ← {b} 是真 workspace，候选 key = "{a}/{b}"
#   2. 未清洗的单层      : works/{bucket}/{name}/   ← {name} 含 ':' '+' '=' 之一
#
# 用法：
#   ./migrate_works_chars.sh --works-dir /data/works                # dry-run（默认）
#   ./migrate_works_chars.sh --works-dir /data/works --apply        # 真正执行
#   ./migrate_works_chars.sh --works-dir /data/works --verify       # 仅校验残留（exit 1 = 仍有残留）
#
# 安全保护：
#   - 对候选语义 key 都会校验 md5(key)[0:2] == bucket，不匹配只告警跳过。
#   - 冲突合并：子目录走 `rsync -a --ignore-existing`；history.jsonl / timer_jobs.json
#     按 mtime 取新，旧的另存为 `<file>.premigration_<ts>`。
#   - 已是合法 sanitized leaf 的目录一律跳过，不动。
#
# 建议先 dry-run 看输出，备份后再 --apply。回滚靠 tar 备份。

set -euo pipefail

WORKS=""
MODE="dry-run"   # dry-run | apply | verify

while [[ $# -gt 0 ]]; do
  case "$1" in
    --works-dir) WORKS="$2"; shift 2 ;;
    --apply)     MODE="apply"; shift ;;
    --verify)    MODE="verify"; shift ;;
    -h|--help)
      sed -n '2,30p' "$0"
      exit 0
      ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [[ -z "$WORKS" ]]; then
  echo "missing --works-dir <path>" >&2
  exit 2
fi
if [[ ! -d "$WORKS" ]]; then
  echo "works dir not found: $WORKS" >&2
  exit 2
fi

# 依赖检查：md5sum / rsync
for bin in md5sum rsync; do
  if ! command -v "$bin" >/dev/null 2>&1; then
    echo "missing dependency: $bin" >&2
    exit 2
  fi
done

md5_first_byte() {
  printf '%s' "$1" | md5sum | cut -c1-2
}

# 与 control::filesystem_workspace_leaf 完全一一对应
sanitize_leaf() {
  local s="$1"
  s="${s//\//_}"
  s="${s//\\/_}"
  s="${s//:/_}"
  s="${s//+/_}"
  s="${s//=/_}"
  printf '%s' "$s"
}

has_bad_char() {
  case "$1" in
    *[/\\:+=]*) return 0 ;;
    *) return 1 ;;
  esac
}

is_workspace_dir() {
  local d="$1"
  [[ -d "$d/memory" || -d "$d/work" || -d "$d/skills" || -d "$d/cases" \
     || -f "$d/history.jsonl" || -f "$d/timer_jobs.json" ]]
}

safe_merge() {
  local src="$1" dst="$2" ts
  ts=$(date +%Y%m%d_%H%M%S)
  mkdir -p "$dst"
  rsync -a --ignore-existing "$src"/ "$dst"/
  local f
  for f in history.jsonl timer_jobs.json; do
    if [[ -f "$src/$f" && -f "$dst/$f" ]]; then
      if [[ "$src/$f" -nt "$dst/$f" ]]; then
        mv "$dst/$f" "$dst/$f.premigration_$ts"
        cp -a "$src/$f" "$dst/$f"
      else
        cp -a "$src/$f" "$dst/$f.premigration_$ts"
      fi
    fi
  done
}

# do_move SRC DST CANDIDATE_KEY
do_move() {
  local src="$1" dst="$2" key="$3"
  case "$MODE" in
    dry-run|verify)
      if [[ -e "$dst" ]]; then
        echo "[MERGE] key='$key'  $src  ->  $dst   (dst exists, will safe-merge)"
      else
        echo "[MOVE]  key='$key'  $src  ->  $dst"
      fi
      ;;
    apply)
      if [[ -e "$dst" ]]; then
        echo "[MERGE] key='$key'  $src  ->  $dst"
        safe_merge "$src" "$dst"
        rm -rf "$src"
      else
        echo "[MOVE]  key='$key'  $src  ->  $dst"
        mkdir -p "$(dirname "$dst")"
        mv "$src" "$dst"
      fi
      ;;
  esac
}

found_nested=0
found_badchars=0
warned=0

shopt -s nullglob

for bucket_dir in "$WORKS"/*/; do
  bucket=$(basename "$bucket_dir")
  for a_dir in "$bucket_dir"*/; do
    [[ -d "$a_dir" ]] || continue
    a=$(basename "$a_dir")

    if is_workspace_dir "$a_dir"; then
      # 形态 2：单层但目录名含 `: + = / \`
      if has_bad_char "$a"; then
        expected=$(md5_first_byte "$a")
        if [[ "$expected" != "$bucket" ]]; then
          echo "[WARN] bucket mismatch single: $a_dir (got=$bucket want=$expected for '$a'), SKIP"
          warned=$((warned + 1))
          continue
        fi
        leaf=$(sanitize_leaf "$a")
        if [[ "$leaf" == "$a" ]]; then
          # 不应发生（has_bad_char 通过但 sanitize 没变化）
          continue
        fi
        dst="$WORKS/$bucket/$leaf"
        do_move "$a_dir" "$dst" "$a"
        found_badchars=$((found_badchars + 1))
      fi
      continue
    fi

    # 形态 1：嵌套，往下钻一层
    for b_dir in "$a_dir"*/; do
      [[ -d "$b_dir" ]] || continue
      b=$(basename "$b_dir")
      is_workspace_dir "$b_dir" || continue

      candidate="${a}/${b}"
      expected=$(md5_first_byte "$candidate")
      if [[ "$expected" != "$bucket" ]]; then
        echo "[WARN] bucket mismatch nested: $b_dir (got=$bucket want=$expected for '$candidate'), SKIP"
        warned=$((warned + 1))
        continue
      fi
      leaf=$(sanitize_leaf "$candidate")
      dst="$WORKS/$bucket/$leaf"
      do_move "$b_dir" "$dst" "$candidate"
      found_nested=$((found_nested + 1))
    done

    # 收尾：apply 模式下若 a_dir 已空则删掉（仅一层空目录）
    if [[ "$MODE" == "apply" ]]; then
      rmdir "$a_dir" 2>/dev/null || true
    fi
  done
done

echo "----"
echo "mode=$MODE nested_found=$found_nested badchars_found=$found_badchars warnings=$warned"

if [[ "$MODE" == "verify" ]]; then
  if [[ $found_nested -gt 0 || $found_badchars -gt 0 ]]; then
    echo "VERIFY FAILED: residual dirs need migration" >&2
    exit 1
  fi
  echo "VERIFY OK: no residual nested/badchars dirs"
fi
