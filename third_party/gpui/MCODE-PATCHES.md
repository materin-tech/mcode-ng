# gpui 0.2.2 本地补丁说明

vendored 自 crates.io `gpui 0.2.2`（Apache-2.0，LICENSE-APACHE 随源码保留），
经 `[patch.crates-io]`（根 Cargo.toml）以路径依赖替换。裁掉了 docs/examples/tests
（path 依赖不参与编译），其余与 registry 版本逐字节一致、外加下列补丁。

## 为什么必须 vendor

Windows 后端的组词（IME composition）从未真正接入应用：主循环
`GetMessageW → DispatchMessageW` 不调 `TranslateMessage`，而 Win32 输入法
认领按键（`ImmProcessKey`）恰恰发生在这步。仓库版只在 `handle_keydown_msg`
的「应用未消费」分支用事后伪造的 MSG 补调——时序上驱动不了 TSF 输入法
（搜狗实测）：组词串永远进不了 input handler，输入框不显示组词、键盘闸门
（`marked_text_range()`）恒为 None，退格/ESC 以裸键命中应用绑定。

## 补丁清单（全部位于 src/platform/windows/）

1. **platform.rs `run()`**：主循环补标准 `TranslateMessage(&msg)`——IME 认领
   按键的唯一正时序。
2. **events.rs `handle_keydown_msg()`**：`VK_PROCESSKEY`（= 该键已被 IME 消费）
   直接拦截返回，**不再**经 `ImmGetVirtualKey` 还原成原始键派发给应用；
   组词中未认领键直接吞掉；删除两处事后 `translate_message` 补调（主循环
   已统一处理，重复调用会双发 WM_CHAR）。
3. **events.rs `handle_key_event()`**：删除 `VK_PROCESSKEY → ImmGetVirtualKey`
   还原逻辑（与「VK_PROCESSKEY = IME 所有」的契约相反）；`VK_PACKET` 分支
   去掉重复 translate。
4. **events.rs `handle_char_msg()`**：过滤控制字符（`\r`/`\x03` 等）——主循环
   TranslateMessage 后控制字符也会生成 WM_CHAR，它们由 keydown 绑定处理，
   只有可打印字符进 input handler。
5. **events.rs `WM_IME_ENDCOMPOSITION`**（新增）：输入法取消组词（非提交）
   时同步清 entity 的 marked，防死组词卡住键盘闸门；提交路径
   （`GCS_RESULTSTR`）已先清 marked，此处对无 marked 必须空转。

## 升级注意

换 gpui 版本时须对照此清单重放补丁；上游 Zed 主线 gpui 的 Windows IME 实现
已大幅演进，升级到主线版本后本目录可整体移除。
