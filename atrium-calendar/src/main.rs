#[cfg(target_os = "freebsd")]
mod ui;

#[cfg(target_os = "freebsd")]
fn main() -> std::io::Result<()> {
    ui::run()
}

#[cfg(not(target_os = "freebsd"))]
fn main() -> std::process::ExitCode {
    eprintln!("Calendar's Pergola window runs on FreeBSD.");
    std::process::ExitCode::FAILURE
}
