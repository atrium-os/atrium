//! Mail's `sync` worker (mail.md §4, app-composition.md §4): one account's
//! IMAP session, in a jail of its own with no capability. The courier's
//! bytes and the store's answers come from the engine on standard input;
//! see `mail_sync::remote`.

fn main() -> std::process::ExitCode {
    let (i, o) = (std::io::stdin().lock(), std::io::stdout().lock());
    match mail_sync::remote::serve(std::io::BufReader::new(i), std::io::BufWriter::new(o)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mail-sync-worker: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
