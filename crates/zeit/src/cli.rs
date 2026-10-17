//! The command line's face: help, usage and errors, styled when they go to a terminal.

use std::io::IsTerminal;

fn styled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

/// `**heading**` bold, `` `name` `` in the accent colour, `<arg>` dim italic.
pub fn render(text: &str) -> String {
    if !styled() {
        return text.replace("**", "").replace('`', "");
    }
    let mut out = String::new();
    let (mut bold, mut code) = (false, false);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                bold = !bold;
                out.push_str(if bold { "\x1b[1m" } else { "\x1b[22m" });
            }
            '`' => {
                code = !code;
                out.push_str(if code { "\x1b[36m" } else { "\x1b[39m" });
            }
            '<' if !code => {
                out.push_str("\x1b[2;3m<");
                for d in chars.by_ref() {
                    out.push(d);
                    if d == '>' {
                        break;
                    }
                }
                out.push_str("\x1b[22;23m");
            }
            c => out.push(c),
        }
    }
    out
}

pub fn print(text: &str) {
    println!("{}", render(text.trim_end()));
}

/// `error: …`, the usage, a hint; exits with 2.
#[allow(dead_code)]
pub fn fail(msg: &str, usage: &str, more: &str) -> ! {
    let red = if std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() { ("\x1b[1;31m", "\x1b[0m") } else { ("", "") };
    eprintln!("{}error:{} {msg}\n", red.0, red.1);
    eprintln!("{}", render(usage.trim_end()).replace("\x1b[36m", "").replace("\x1b[39m", ""));
    if !more.is_empty() {
        eprintln!("\nFor more, try '{more}'.");
    }
    std::process::exit(2);
}

