//! `speakable <en|hi>`: print the spoken form vcd gives the engine for the text on stdin.
//! A debugging aid for callers ("what will it say?") and for the clarity measurements.

use std::io::Read;

fn main() -> std::process::ExitCode {
    let lang = std::env::args().nth(1).unwrap_or_else(|| "en".to_owned());
    let Ok(language) = lang.parse::<vc_core::Language>() else {
        eprintln!("usage: speakable <en|hi> < text");
        return std::process::ExitCode::from(2);
    };
    let mut text = String::new();
    if std::io::stdin().read_to_string(&mut text).is_err() {
        eprintln!("stdin is not UTF-8");
        return std::process::ExitCode::from(2);
    }
    println!("{}", vc_text::speakable(&text, language));
    std::process::ExitCode::SUCCESS
}
