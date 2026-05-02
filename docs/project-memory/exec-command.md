# 命令执行工具

## 模块目标

- [用户确认] ExecCommand 的模型提示必须兼容 Windows PowerShell 5.1，避免首次生成含 `&&` 的无效命令。

## 规划基线

- P1（完成）：修正依赖命令的 Shell 提示。
- P2（完成）：增加回归测试并执行验证。
- 进度：2/2 完成。

## 决策记录

- D1（2026-05-03，用户确认）：依赖命令应使用当前 Shell 支持的语法；Windows PowerShell 5.1 使用 `; if ($?) { ... }`，不推荐 `&&`。影响：ExecCommand 工具描述。

## 开发状态

- [代码证据] `crates/aion-tools/src/exec_command.rs` 已改为按当前 Shell 选择串联语法，并明确 PowerShell 5.1 写法。
- [代码证据] `crates/aion-tools/src/exec_command_test.rs` 已覆盖提示回归。

## 继续入口

- [代码证据] 本项已完成；验证命令：`cargo test -p aion-tools description_uses_windows_powershell_compatible_chaining_guidance`。
