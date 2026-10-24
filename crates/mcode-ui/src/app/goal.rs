// MCode Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! goal 外层循环 — 实现已下沉 core（[`mcode_core::agent::goal`]），
//! CLI 与 Desktop 共用同一份提示词与裁判逻辑；此处仅 re-export。

pub use mcode_core::agent::goal::{MAX_GOAL_ROUNDS, build_goal_prompt, check_goal_complete};
