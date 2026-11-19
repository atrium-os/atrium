//! Reconcile Later with the account's private server mailbox (§11.4).
//! The record is written before a move. The next sync can recognize either
//! side even when the server applied the move and the reply was lost.

use std::collections::{HashMap, HashSet};

use mail_store::later::{header_hash, Parked};
use mail_store::senders::why;
use mail_store::{FolderId, Hash, MsgId, Role};
use mail_sync::SnoozeMove;

use crate::{now, AccountSink, Error};

type ParkedLinks = (HashSet<MsgId>, HashMap<(Hash, u64), Option<MsgId>>);

impl AccountSink<'_> {
    pub(crate) fn wants_parked_mailbox(&self) -> Result<bool, Error> {
        let active = self.active_later()?;
        for (msg, _, _, why) in self.e.store.held(self.account)? {
            if why == why::LATER && active.contains(&self.e.store.message(msg)?.thread) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn active_later(&self) -> Result<HashSet<mail_store::ThreadId>, Error> {
        let at = now();
        Ok(self
            .e
            .store
            .later_items(self.account)?
            .into_iter()
            .filter(|(_, l)| !l.back && l.due > at)
            .map(|(_, l)| l.thread)
            .collect())
    }

    pub(crate) fn plan_snoozes(&self) -> Result<Vec<SnoozeMove>, Error> {
        let Some((parked_folder, parked_name)) = &self.snoozed else {
            return Ok(Vec::new());
        };
        let store = &self.e.store;
        let folders: HashMap<_, _> = store.folders(self.account)?.into_iter().collect();
        let accounts_later = self.active_later()?;
        let mut tracking: HashMap<_, _> = store.parked(self.account)?.into_iter().collect();
        let mut fingerprints: HashSet<_> = tracking.values().map(|p| (p.header, p.size)).collect();
        let mut add = Vec::new();
        let mut forget = Vec::new();
        let mut moves = Vec::new();
        for (msg, inbox, uid, reason) in store.held(self.account)? {
            let Some(folder) = folders
                .get(&inbox)
                .filter(|f| f.role == Role::Inbox && !f.hidden)
            else {
                continue;
            };
            if reason != why::LATER || store.op_pending_at(inbox, uid)? {
                continue;
            }
            let m = store.message(msg)?;
            if !accounts_later.contains(&m.thread)
                || tracking.get(&msg).is_some_and(|p| p.inbox != inbox)
            {
                continue;
            }
            if m.locations.iter().any(|(f, _)| f == parked_folder) {
                continue;
            }
            if !tracking.contains_key(&msg) {
                let header = header_hash(&store.blob(&m.headers)?);
                if !fingerprints.insert((header, m.size)) && m.server_id.is_none() {
                    // Two byte-identical deliveries cannot both be mapped
                    // back to their original UIDs after an interrupted move.
                    continue;
                }
                let park = Parked {
                    inbox,
                    header,
                    size: m.size,
                    returning: false,
                };
                tracking.insert(msg, park.clone());
                add.push((msg, park));
            } else if let Some(park) = tracking.get_mut(&msg).filter(|park| park.returning) {
                park.returning = false;
                add.push((msg, park.clone()));
            }
            moves.push(SnoozeMove {
                folder: folder.name.clone(),
                uid,
                to: parked_name.clone(),
                returning: false,
            });
        }
        for (id, mut park) in tracking {
            let m = store.message(id)?;
            if accounts_later.contains(&m.thread) {
                continue;
            }
            if let Some(&(_, uid)) = m.locations.iter().find(|(f, _)| f == parked_folder) {
                if let Some(inbox) = folders
                    .get(&park.inbox)
                    .filter(|f| f.role == Role::Inbox && !f.hidden)
                {
                    if !store.op_pending_at(*parked_folder, uid)? {
                        if !park.returning {
                            park.returning = true;
                            add.push((id, park.clone()));
                        }
                        moves.push(SnoozeMove {
                            folder: parked_name.clone(),
                            uid,
                            to: inbox.name.clone(),
                            returning: true,
                        });
                    }
                }
            } else {
                // Both mailboxes were visited in this pass. This copy is
                // home already or was removed independently on the server.
                forget.push(id);
            }
        }
        if !add.is_empty() || !forget.is_empty() {
            store.write(|w| {
                for (msg, park) in add {
                    w.park(self.account, msg, &park)?;
                }
                for msg in forget {
                    w.forget_parked(self.account, msg)?;
                }
                Ok(())
            })?;
        }
        Ok(moves)
    }

    /// A folder-to-folder transfer must reuse the original message ID.
    /// JMAP has an account-wide ID. For IMAP, link only a unique exact
    /// header/size match that doesn't already occupy the destination.
    pub(crate) fn parked_links(&self, folder: FolderId) -> Result<ParkedLinks, Error> {
        let Some((parked, _)) = self.snoozed else {
            return Ok(Default::default());
        };
        let mut tracked = HashSet::new();
        let mut links = HashMap::new();
        for (id, route) in self.e.store.parked(self.account)? {
            if folder != parked && !(route.returning && folder == route.inbox) {
                continue;
            }
            let m = self.e.store.message(id)?;
            if m.locations
                .iter()
                .chain(&m.held)
                .chain(&m.leaving)
                .any(|(f, _)| *f == folder)
            {
                continue;
            }
            tracked.insert(id);
            links
                .entry((route.header, route.size))
                .and_modify(|found| *found = None)
                .or_insert(Some(id));
        }
        Ok((tracked, links))
    }
}
