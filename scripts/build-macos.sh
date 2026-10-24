#!/usr/bin/env bash
# 构建 macOS .app（Apple Silicon；Intel 加 --target x86_64-apple-darwin 出双架构）
# Info.plist 以 crates/mcode-ui/macos/Info.plist 为唯一事实源，此处仅替换版本占位。
set -euo pipefail
cd "$(dirname "$0")/.."
MCODE_APP_VERSION="${1:-$(sed -n 's/^[[:space:]]*version = "\(.*\)"/\1/p' Cargo.toml | head -1)}"
MCODE_APP_VERSION="${MCODE_APP_VERSION#v}"
cargo build --release -p mcode-ui
APP="target/release/bundle/MCode_Desktop.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
# 内置资产（ecc-core agents/skills + pua-core skills）随包分发，运行时物化到 ~/.mcode/assets
cp -R assets "$APP/Contents/Resources/assets"
# 应用图标（scripts/make_icons.py 生成）
cp assets/brand/icon.icns "$APP/Contents/Resources/icon.icns"
cp target/release/MCode_Desktop "$APP/Contents/MacOS/"
sed "s/0\.0\.0-PLACEHOLDER/${MCODE_APP_VERSION}/g" \
  crates/mcode-ui/macos/Info.plist > "$APP/Contents/Info.plist"
codesign --force --deep --sign - "$APP" 2>/dev/null || true
echo "✓ 产出: $APP"
