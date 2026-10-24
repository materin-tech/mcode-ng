# 构建 Windows 发行版（x64）：cargo build + 便携 zip
# 前置：Rust stable-msvc + VS Build Tools（C++ 工作负载）+ Direct3D 11（系统自带）
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
cargo build --release -p mcode-ui --target x86_64-pc-windows-msvc
$out = "target/release/bundle/MCode_Desktop-win64"
New-Item -ItemType Directory -Force -Path $out | Out-Null
Copy-Item "target/x86_64-pc-windows-msvc/release/MCode_Desktop.exe" $out
# 图标/品牌资产随包（exe 内嵌图标经 build.rs winresource；assets 供运行时物化）
Copy-Item -Recurse -Force "assets" "$out/assets"
Copy-Item "assets/brand/icon.ico" "$out/MCode_Desktop.ico"
Compress-Archive -Force -Path "$out/*" -DestinationPath "target/release/bundle/MCode_Desktop-win64.zip"
Write-Host "OK: target/release/bundle/MCode_Desktop-win64.zip"
# 可选 MSI：cargo install cargo-wix && cargo wix -p mcode-ui
