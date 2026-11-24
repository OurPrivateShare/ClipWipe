use crate::{Mode, WatchOptions};
use std::time::Duration;

#[cfg(windows)]
const PLATFORM_TITLE: &str = "Windows 剪贴板清理工具\n\n";
#[cfg(target_os = "linux")]
const PLATFORM_TITLE: &str = "Linux 剪贴板清理工具\n\n";

#[cfg(windows)]
const INSTALL_EXAMPLE: &str = "\
  安装，在管理员 PowerShell 中进入解压目录后运行
    .\\clipwipe.exe install
";
#[cfg(target_os = "linux")]
const INSTALL_EXAMPLE: &str = "\
  安装，在解压目录运行
    sudo ./clipwipe install
";

#[cfg(windows)]
const INSTALL_CHECK_EXAMPLE: &str = "\
  安装后验证，从开始菜单重新打开终端
    clipwipe --version
";
#[cfg(target_os = "linux")]
const INSTALL_CHECK_EXAMPLE: &str = "\
  安装后验证
    clipwipe --version
";

#[cfg(windows)]
const UNINSTALL_EXAMPLE: &str = "\
  卸载，在管理员终端运行
    clipwipe uninstall
";
#[cfg(target_os = "linux")]
const UNINSTALL_EXAMPLE: &str = "\
  卸载
    sudo /usr/local/bin/clipwipe uninstall
";

const HELP: &[&str] = &[
    PLATFORM_TITLE,
    "\
用法：clipwipe [命令]

  clear [--mode clear|random]                  立即处理一次
  watch [--timeout 秒] [--mode clear|random]    持续监听，按 Ctrl+C 退出
  install                                       安装到系统，供所有用户使用
  uninstall                                     卸载系统安装
  autostart enable [--timeout 秒] [--mode clear|random]
                                                下次登录图形桌面时启动
  autostart disable                             取消登录自启
  -h, --help                                    显示帮助
  -V, --version                                 显示版本

默认每次复制后等待 30 秒再清空。random 将剪贴板替换为 32 个随机十六进制字符。

常用命令（可直接复制）

",
    INSTALL_EXAMPLE,
    "\n",
    INSTALL_CHECK_EXAMPLE,
    "\n",
    "\
  设置当前用户登录自启，使用默认 30 秒
    clipwipe autostart enable
  设置当前用户登录自启，60 秒后随机覆盖
    clipwipe autostart enable --timeout 60 --mode random
  取消当前用户登录自启
    clipwipe autostart disable

  立即清空 / 随机覆盖
    clipwipe clear
    clipwipe clear --mode random
  前台监听，15 秒后清空
    clipwipe watch --timeout 15

",
    UNINSTALL_EXAMPLE,
];

const CLEAR_HELP: &str =
    "用法：clipwipe clear [--mode clear|random]\n立即清空剪贴板，或用随机文本覆盖。\n";
const WATCH_HELP: &str = "用法：clipwipe watch [--timeout 秒] [--mode clear|random]\n每次复制重新计时，默认 30 秒并清空。按 Ctrl+C 退出。\n";
const INSTALL_HELP: &[&str] = &[
    "\
用法：clipwipe install
将程序安装到系统，供所有用户在终端使用。

",
    INSTALL_EXAMPLE,
    "\n",
    INSTALL_CHECK_EXAMPLE,
];
const UNINSTALL_HELP: &[&str] = &[
    "\
用法：clipwipe uninstall
卸载系统安装。先以日常用户身份关闭自己的登录自启。

    clipwipe autostart disable

",
    UNINSTALL_EXAMPLE,
];
const AUTOSTART_HELP: &str = "\
登录自启作用于当前用户，请在日常使用的终端运行。

  默认 30 秒后清空
    clipwipe autostart enable
  60 秒后随机覆盖
    clipwipe autostart enable --timeout 60 --mode random
  取消登录自启
    clipwipe autostart disable
";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Help(&'static [&'static str]),
    Version,
    Clear(Mode),
    Watch(WatchOptions),
    Install,
    Uninstall,
    Enable(WatchOptions),
    Disable,
    #[cfg(target_os = "linux")]
    HoldRandom,
}

pub(crate) fn parse(arguments: &[String]) -> Result<Command, String> {
    let args: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let command = match args.as_slice() {
        [] | ["-h" | "--help" | "help"] => Command::Help(HELP),
        ["-V" | "--version"] => Command::Version,
        ["clear", "-h" | "--help"] => Command::Help(&[CLEAR_HELP]),
        ["watch", "-h" | "--help"] => Command::Help(&[WATCH_HELP]),
        ["install", "-h" | "--help"] => Command::Help(INSTALL_HELP),
        ["uninstall", "-h" | "--help"] => Command::Help(UNINSTALL_HELP),
        ["autostart", "-h" | "--help"] | ["autostart", "enable" | "disable", "-h" | "--help"] => {
            Command::Help(&[AUTOSTART_HELP])
        }
        ["clear", options @ ..] => Command::Clear(parse_options(options, false)?.mode),
        ["watch", options @ ..] => Command::Watch(parse_options(options, true)?),
        ["install"] => Command::Install,
        ["uninstall"] => Command::Uninstall,
        ["autostart", "enable", options @ ..] => Command::Enable(parse_options(options, true)?),
        ["autostart", "disable"] => Command::Disable,
        ["autostart"] => return Err("autostart 需要指定 enable 或 disable".into()),
        #[cfg(target_os = "linux")]
        ["__hold-random"] => Command::HoldRandom,
        ["install" | "uninstall" | "autostart", unexpected, ..] => {
            return Err(format!("无法识别参数 {unexpected:?}"));
        }
        [unexpected, ..] => return Err(format!("无法识别命令 {unexpected:?}")),
    };
    Ok(command)
}

fn parse_options(mut args: &[&str], allow_timeout: bool) -> Result<WatchOptions, String> {
    let mut options = WatchOptions::default();
    let mut mode_seen = false;
    let mut timeout_seen = false;
    while let Some((&flag, tail)) = args.split_first() {
        let (name, value, rest) = match flag.split_once('=') {
            Some((name, value)) => (name, value, tail),
            None => {
                let Some((&value, remaining)) = tail.split_first() else {
                    return Err(format!("参数 {flag:?} 缺少值或无法识别"));
                };
                (flag, value, remaining)
            }
        };
        match name {
            "--mode" if !mode_seen => {
                options.mode = match value {
                    "clear" => Mode::Clear,
                    "random" => Mode::Random,
                    _ => return Err("--mode 必须是 clear 或 random".into()),
                };
                mode_seen = true;
            }
            "--timeout" if allow_timeout && !timeout_seen => {
                let seconds = value
                    .parse::<u64>()
                    .ok()
                    .filter(|seconds| *seconds > 0)
                    .ok_or("--timeout 必须是正整数秒")?;
                options.timeout = Duration::from_secs(seconds);
                timeout_seen = true;
            }
            _ => return Err(format!("参数 {name:?} 无法识别或重复")),
        }
        args = rest;
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Command, String> {
        parse(&args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn no_arguments_show_help() {
        let Command::Help(sections) = parse_args(&[]).unwrap() else {
            panic!("expected the main help");
        };
        let help = sections.concat();
        for example in ["clipwipe autostart enable", "clipwipe autostart disable"] {
            assert!(help.lines().any(|line| line.trim() == example));
        }
        assert_eq!(parse_args(&["--version"]), Ok(Command::Version));
        assert_eq!(
            parse_args(&["watch"]),
            Ok(Command::Watch(WatchOptions::default()))
        );
    }

    #[test]
    fn help_shows_only_current_platform_commands() {
        #[cfg(windows)]
        let (install, uninstall, other_platform) = (
            r".\clipwipe.exe install",
            "clipwipe uninstall",
            ["Linux", "sudo "],
        );
        #[cfg(target_os = "linux")]
        let (install, uninstall, other_platform) = (
            "sudo ./clipwipe install",
            "sudo /usr/local/bin/clipwipe uninstall",
            ["Windows", ".exe"],
        );

        for (args, expected) in [
            (&["--help"][..], &[install, uninstall][..]),
            (&["install", "--help"], &[install]),
            (&["uninstall", "--help"], &[uninstall]),
        ] {
            let Command::Help(sections) = parse_args(args).unwrap() else {
                panic!("expected help for {args:?}");
            };
            let help = sections.concat();
            for example in expected {
                assert!(help.lines().any(|line| line.trim() == *example), "{args:?}");
            }
            for marker in other_platform {
                assert!(!help.contains(marker), "{args:?} contains {marker:?}");
            }
        }
    }

    #[test]
    fn parses_clear_and_autostart_options() {
        assert_eq!(
            parse_args(&["clear", "--mode=random"]),
            Ok(Command::Clear(Mode::Random))
        );
        assert_eq!(
            parse_args(&["autostart", "enable", "--timeout", "60", "--mode", "random"]),
            Ok(Command::Enable(WatchOptions {
                timeout: Duration::from_secs(60),
                mode: Mode::Random
            }))
        );
    }

    #[test]
    fn rejects_invalid_and_misplaced_options() {
        for args in [
            &["watch", "--timeout", "0"][..],
            &["watch", "--timeout", "-1"],
            &["watch", "--timeout", "18446744073709551616"],
            &["watch", "--timeout"],
            &["watch", "--mode=random", "--mode=clear"],
            &["clear", "--timeout", "30"],
            &["autostart", "disable", "--mode", "clear"],
        ] {
            assert!(parse_args(args).is_err(), "unexpected success for {args:?}");
        }
    }
}
