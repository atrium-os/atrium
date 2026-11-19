//! Report phishing to the company (mail.md §7.7): in a persona with a
//! report address, Report phishing also sends the message there, attached
//! as it arrived (headers and all, which is what a security team reads),
//! through the same outbox as any send, so Undo takes it back.

use mail_compose::{Attachment, Draft, Mailbox};
use mail_store::MsgId;

use crate::send::When;
use crate::{Engine, Error};

/// Most messages in one report (a conversation's worth).
pub const REPORT_MAX: usize = 20;

impl Engine {
    /// Send `msgs` to `to` as a phishing report, from the first one's
    /// account; the send's id. `None` when none of them is here in full
    /// (only its headers were fetched), so there is nothing to attach.
    pub fn report_phishing(&self, msgs: &[MsgId], to: &str) -> Result<Option<u64>, Error> {
        let to = crate::prefs::report_address(to).ok_or_else(|| Error::Compose("not a report address".into()))?;
        let mut account = None;
        let mut subject = String::new();
        let mut attachments = Vec::new();
        for &id in msgs.iter().take(REPORT_MAX) {
            let m = self.store.message(id).map_err(|_| Error::NotFound)?;
            if account.is_some_and(|a| a != m.account) {
                continue;
            }
            let Some(full) = m.full else { continue };
            account = Some(m.account);
            if subject.is_empty() {
                subject = m.summary.subject.clone();
            }
            let n = attachments.len() + 1;
            let name = if n == 1 { "message.eml".to_string() } else { format!("message-{n}.eml") };
            attachments.push(Attachment { name, mime: "message/rfc822".into(), bytes: self.store.blob(&full)? });
        }
        let Some(account) = account else { return Ok(None) };
        let from = self.identities(account)?.into_iter().next().ok_or(Error::NotFound)?;
        let n = attachments.len();
        let draft = Draft {
            from: Mailbox::new(&from.name, &from.addr),
            to: vec![Mailbox::new("", &to)],
            subject: format!("Phishing report: {}", if subject.trim().is_empty() { "(no subject)" } else { subject.trim() }),
            text: format!(
                "Reported as phishing from Mail. The {} attached as {} arrived, with {} headers.\n",
                if n == 1 { "message is" } else { "messages are" },
                if n == 1 { "it" } else { "they" },
                if n == 1 { "its" } else { "their" }
            ),
            attachments,
            ..Draft::default()
        };
        self.send(account, &draft, When::default()).map(Some)
    }
}
