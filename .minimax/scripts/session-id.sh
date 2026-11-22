#!/bin/bash
# session-id.sh —— 用当前存档的初始 .sav 文件 hash 作为单局记忆的稳定 key
# 为什么 hash 初始存档而不是 auto_save：
#   初始存档在同一局内永不变（auto_save 每次存盘都变）→ hash 稳定；
#   换局/重开 → 初始存档不同 → hash 不同 → 自动切换到新记忆。
#
# 用法：./.minimax/scripts/session-id.sh  输出形如 743caeb9fcc28503（16位短hash）
set -e
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ONI="$ROOT/target/release/oni"
if [ ! -x "$ONI" ]; then echo "错误: 未找到 $ONI，先 cargo build --release" >&2; exit 1; fi

# 通过 mod 拿当前活动存档的初始文件路径（activeSaveFile 字段）
INFO="$($ONI call game_control '{"domain":"save","action":"list","confirm":false,"task":"session-id: 取活动存档"}' 2>/dev/null || true)"
# activeSaveFile 形如 .../<save-name>/<save-name>.sav —— 提取之
SAVE="$(printf "%s" "$INFO" | sed -n 's/^activeSaveFile: //p' | head -n 1)"
if [ -z "$SAVE" ] || [ ! -f "$SAVE" ]; then
  echo "错误: 无法确定活动存档（游戏未加载？）" >&2; exit 1;
fi
# 算初始存档 hash（取前16位）—— 这一局内稳定不变
shasum -a 256 "$SAVE" | awk '{print substr($1,1,16)}'
