// MCode_Desktop — AI coding agent desktop app
// Copyright (c) 2026 Materin-tech. All rights reserved.
// SPDX-License-Identifier: MIT

//! @ 引用体验：层级下钻候选（对齐 CLI）与“@即选中”文件树联动的状态机测试。

use crate::app::at_candidates_from_tree;

/// 候选只列查询目录的立即子项（层级下钻），不做整棵树扁平 substring 全表扫：
/// query "src/v" 只命中 src/ 下的 v* 子项；顶层 query 命中顶层子项。
#[test]
fn at_candidates_drill_down_by_directory_prefix() {
    let tree = vec![
        "src/".to_string(),
        "src/api/".to_string(),
        "src/api/client.ts".to_string(),
        "src/views/".to_string(),
        "src/views/chat_view.rs".to_string(),
        "README.md".to_string(),
    ];

    // 顶层：只列顶层子项
    let top = at_candidates_from_tree(&tree, "");
    let names: Vec<&str> = top.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["src", "README.md"]);
    assert!(top[0].is_dir && !top[1].is_dir);

    // 下钻：query "src/v" → 仅 src/ 下的 v* 立即子项
    let drill = at_candidates_from_tree(&tree, "src/v");
    assert_eq!(
        drill.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        vec!["src/views"]
    );
    assert!(drill[0].is_dir);

    // 文件命中：query "src/api/c" → client.ts
    let file = at_candidates_from_tree(&tree, "src/api/c");
    assert_eq!(
        file.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        vec!["src/api/client.ts"]
    );

    // 目录候选名不带尾随 '/'（apply 后产生合法 @token，extract_refs 可解析）
    assert!(drill.iter().all(|c| !c.name.ends_with('/')));
}

/// 无匹配 → 空候选；目录前缀在树中不存在 → 空候选。
#[test]
fn at_candidates_empty_cases() {
    let tree = vec!["src/".to_string(), "src/main.rs".to_string()];
    assert!(at_candidates_from_tree(&tree, "zzz").is_empty());
    assert!(at_candidates_from_tree(&tree, "nope/").is_empty());
}
