#!/bin/bash
# PRD app-tiering: 再生成 tray/assets/cjk-subset.ttf —
# 扫描 tray/src/*.rs 全部非 ASCII 字符, pyftsubset 子集化 Hiragino Sans GB。
# 全量系统字体 22.4MB 会驻留 3 份 malloc 拷贝; 子集 ~120KB, include_bytes 内嵌。
# 新增中文标签/注释后跑一次: bash tray/gen_font_subset.sh && cargo test -p fusion-tray
# 覆盖回归测试: font_tests::test_subset_font_covers_source_charset。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VENV="${FUSION_VENV:-$ROOT/../.venv}"
# shellcheck disable=SC1091
source "$VENV/bin/activate"
CHARSET="/tmp/fusion-tray-charset.txt"
python3 - "$ROOT" "$CHARSET" <<'EOF'
import glob, pathlib, sys
root, out = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
chars = set()
for f in glob.glob(str(root / 'tray/src/*.rs')):
    chars |= {c for c in pathlib.Path(f).read_text(encoding='utf-8') if ord(c) > 0x7E}
out.write_text(''.join(sorted(chars)), encoding='utf-8')
print(len(chars), 'non-ascii chars ->', out)
EOF
pyftsubset "/System/Library/Fonts/Hiragino Sans GB.ttc" \
    --font-number=0 \
    --text-file="$CHARSET" \
    --unicodes="U+0020-007E" \
    --layout-features='' \
    --no-hinting \
    --name-IDs='' \
    --drop-tables+=DSIG,BASE,vhea,vmtx \
    --output-file="$ROOT/tray/assets/cjk-subset.ttf"
stat -f '%z bytes %N' "$ROOT/tray/assets/cjk-subset.ttf"
