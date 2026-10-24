//! mcode CLI 入口：交互 TUI（默认）/ print 一次性模式（-p）。
//!
//! 手写参数解析（不引 clap）：--print/--model/--fallback-model/--continue/
//! --resume/--version/--help。

mod print_mode;
mod session_setup;
mod tui;

use tui::Resume;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse(&args) {
        Ok(Cli::Version) => {
            println!("mcode {}", env!("CARGO_PKG_VERSION"));
        }
        Ok(Cli::Help) => {
            print_help();
        }
        Ok(Cli::Tui {
            model,
            fallback,
            resume,
        }) => {
            let rt = tokio::runtime::Runtime::new().expect("启动 tokio runtime");
            if let Err(e) = rt.block_on(tui::run(model.as_deref(), fallback, resume)) {
                eprintln!("mcode：{e:#}");
                std::process::exit(1);
            }
        }
        Ok(Cli::Print {
            prompt,
            model,
            fallback,
            resume,
        }) => {
            let rt = tokio::runtime::Runtime::new().expect("启动 tokio runtime");
            if let Err(e) = rt.block_on(print_mode::run_print(
                &prompt,
                model.as_deref(),
                fallback,
                resume,
            )) {
                eprintln!("mcode：{e:#}");
                std::process::exit(1);
            }
        }
        Err(msg) => {
            eprintln!("mcode：{msg}\n");
            print_help();
            std::process::exit(2);
        }
    }
}

#[derive(Debug)]
enum Cli {
    Tui {
        model: Option<String>,
        fallback: Option<Vec<String>>,
        resume: Resume,
    },
    Print {
        prompt: String,
        model: Option<String>,
        fallback: Option<Vec<String>>,
        resume: Resume,
    },
    Version,
    Help,
}

fn parse(args: &[String]) -> Result<Cli, String> {
    let mut model: Option<String> = None;
    let mut fallback: Option<Vec<String>> = None;
    let mut print_prompt: Option<String> = None;
    let mut continue_last = false;
    let mut pick = false;
    let mut resume_id: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-h" | "--help" => return Ok(Cli::Help),
            "-V" | "--version" => return Ok(Cli::Version),
            "-p" | "--print" => {
                // 后续剩余参数整体作为 prompt（对齐 CC 的多词 prompt 习惯）
                let rest: Vec<&String> = args[i + 1..].iter().collect();
                if rest.is_empty() {
                    return Err("--print 需要 prompt 参数".into());
                }
                print_prompt = Some(
                    rest.iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                );
                i = args.len();
                continue;
            }
            "--model" | "-m" => {
                let Some(m) = args.get(i + 1) else {
                    return Err("--model 需要模型名".into());
                };
                model = Some(m.clone());
                i += 2;
                continue;
            }
            "--fallback-model" => {
                let Some(f) = args.get(i + 1) else {
                    return Err("--fallback-model 需要模型名".into());
                };
                // 逗号分隔的多级链，单个值 = 一级
                fallback = Some(
                    f.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect(),
                );
                i += 2;
                continue;
            }
            "--continue" | "-c" => {
                continue_last = true;
                i += 1;
                continue;
            }
            "--resume" | "-r" => {
                match args.get(i + 1) {
                    // 非选项值 = 指定 id；否则 = 列出会话
                    Some(v) if !v.starts_with('-') => {
                        resume_id = Some(v.clone());
                        i += 2;
                    }
                    _ => {
                        pick = true;
                        i += 1;
                    }
                }
                continue;
            }
            other => {
                if other.starts_with('-') {
                    return Err(format!("未知参数 {other}"));
                }
                positional.push(other.to_string());
            }
        }
        i += 1;
    }

    // 优先级：--resume <id> > --resume > --continue
    let resume = if let Some(id) = resume_id {
        Resume::Id(id)
    } else if pick {
        Resume::Pick
    } else if continue_last {
        Resume::Continue
    } else {
        Resume::Fresh
    };

    if let Some(prompt) = print_prompt {
        return Ok(Cli::Print {
            prompt,
            model,
            fallback,
            resume,
        });
    }
    // 位置参数兜底：有裸 prompt 也走 print（CC 同款习惯）
    if !positional.is_empty() {
        return Ok(Cli::Print {
            prompt: positional.join(" "),
            model,
            fallback,
            resume,
        });
    }
    Ok(Cli::Tui {
        model,
        fallback,
        resume,
    })
}

fn print_help() {
    println!(
        "mcode {} — 终端编码代理

用法：
  mcode                      交互 TUI
  mcode -c                   继续本项目最近会话
  mcode -r [<id>]            恢复指定会话；无 id 列出可选会话
  mcode -p \"<prompt>\"        print 模式：一次性执行后退出
  mcode --model <name>       覆盖本次会话模型

选项：
  -c, --continue             继续最近会话
  -r, --resume [<id>]        恢复会话（无 id = 列出）
  -p, --print <prompt>       非交互执行
  -m, --model <name>         指定模型
      --fallback-model <m[,m2]>   主模型失败后的降级链
  -V, --version              版本号
  -h, --help                 帮助",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_print_model_and_positional() {
        assert!(matches!(
            parse(&[]),
            Ok(Cli::Tui {
                resume: Resume::Fresh,
                ..
            })
        ));
        let m = parse(&v(&["--model", "glm-5.3"]));
        assert!(
            matches!(m, Ok(Cli::Tui { model: Some(ref s), resume: Resume::Fresh, .. }) if s == "glm-5.3"),
            "{m:?}"
        );

        let p = parse(&v(&["-p", "修", "一下", "bug"]));
        assert!(
            matches!(&p, Ok(Cli::Print { prompt, resume: Resume::Fresh, .. }) if prompt == "修 一下 bug"),
            "{p:?}"
        );

        // 裸 prompt 走 print
        let p = parse(&v(&["跑测试"]));
        assert!(matches!(&p, Ok(Cli::Print { prompt, .. }) if prompt == "跑测试"));

        // print + model 组合（--model 在 -p 前）
        let p = parse(&v(&["--model", "m2", "-p", "hi"]));
        assert!(
            matches!(&p, Ok(Cli::Print { prompt, model: Some(ref m), .. }) if prompt == "hi" && m == "m2")
        );

        assert!(parse(&v(&["--wat"])).is_err());
        assert!(parse(&v(&["--model"])).is_err());
        assert!(matches!(parse(&v(&["--version"])), Ok(Cli::Version)));
    }

    #[test]
    fn parses_resume_continue_fallback() {
        // --continue
        let c = parse(&v(&["--continue"]));
        assert!(matches!(
            c,
            Ok(Cli::Tui {
                resume: Resume::Continue,
                ..
            })
        ));

        // -c 短旗标 + -p 组合
        let c = parse(&v(&["-c", "-p", "继续修"]));
        assert!(
            matches!(&c, Ok(Cli::Print { prompt, resume: Resume::Continue, .. }) if prompt == "继续修"),
            "{c:?}"
        );

        // --resume 无 id = Pick
        let r = parse(&v(&["--resume"]));
        assert!(matches!(
            r,
            Ok(Cli::Tui {
                resume: Resume::Pick,
                ..
            })
        ));

        // --resume <id>
        let r = parse(&v(&["--resume", "abc-123"]));
        assert!(
            matches!(&r, Ok(Cli::Tui { resume: Resume::Id(id), .. }) if id == "abc-123"),
            "{r:?}"
        );

        // -r 短旗标同义
        let r = parse(&v(&["-r", "sid"]));
        assert!(matches!(r, Ok(Cli::Tui { resume: Resume::Id(id), .. }) if id == "sid"));

        // resume 优先级高于 continue
        let r = parse(&v(&["--continue", "--resume", "sid"]));
        assert!(matches!(
            r,
            Ok(Cli::Tui {
                resume: Resume::Id(_),
                ..
            })
        ));

        // --fallback-model 逗号链
        let f = parse(&v(&["--fallback-model", "glm-5.3,deepseek-chat"]));
        assert!(
            matches!(f, Ok(Cli::Tui { fallback: Some(ref fs), .. })
                if fs == &vec!["glm-5.3".to_string(), "deepseek-chat".to_string()]),
            "{f:?}"
        );

        // --fallback-model 紧跟 -p：只吃一个值，剩余归 print
        let f = parse(&v(&["--fallback-model", "m2", "-p", "hi"]));
        assert!(
            matches!(&f, Ok(Cli::Print { fallback: Some(ref fs), prompt, .. })
                if fs == &vec!["m2".to_string()] && prompt == "hi"),
            "{f:?}"
        );

        // --resume 后跟其他选项：resume 转 Pick，选项继续解析
        let r = parse(&v(&["--resume", "--model", "m1"]));
        assert!(
            matches!(r, Ok(Cli::Tui { resume: Resume::Pick, model: Some(ref m), .. }) if m == "m1"),
            "{r:?}"
        );
    }
}
