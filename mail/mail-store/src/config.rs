//! How Mail reaches an account (mail.md §5.1, §5.4): its servers, which
//! Arca items log in (references only: Mail never holds a secret), and how
//! the last login went. Nothing secret is stored here.

use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::{dec, enc, AccountId, Result, Store, Writer};

pub(crate) const CONFIG: TableDefinition<u32, &[u8]> = TableDefinition::new("account_config");

/// One server: `scheme` is Arca's name for it (`imaps`, `imap`,
/// `submissions`, `submission`, `https`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    Imap { imap: Server, smtp: Server },
    /// The session resource's URL (`https://host/.well-known/jmap` or the
    /// provider's own).
    Jmap { session: String },
}

/// How the last attempt to reach the account went (§5.4: "whether the
/// courier last logged in").
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Reached {
    #[default]
    Never,
    Ok { at: i64 },
    /// The server or the provider refused the sign-in: the person signs in
    /// again.
    SignInAgain { at: i64 },
    /// Couldn't reach it (offline, Arca locked, …): tried again later.
    Failed { at: i64, why: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountConfig {
    pub address: String,
    /// The person's name on mail they send.
    pub name: String,
    pub protocol: Protocol,
    /// Arca items (ids) that log in to the incoming and outgoing servers.
    pub incoming_item: Option<[u8; 16]>,
    pub outgoing_item: Option<[u8; 16]>,
    /// The OAuth provider, when the items are OAuth grants.
    pub oauth: Option<String>,
    pub reached: Reached,
}

impl Store {
    pub fn account_config(&self, account: AccountId) -> Result<Option<AccountConfig>> {
        let tx = self.db.begin_read()?;
        let t = tx.open_table(CONFIG)?;
        let v = t.get(account)?;
        v.map(|v| dec(v.value())).transpose()
    }

    /// A new account with its settings: its id.
    pub fn add_configured(&self, c: &AccountConfig) -> Result<AccountId> {
        let id = self.add_account(&c.address)?;
        self.write(|w| w.set_account_config(id, c))?;
        Ok(id)
    }
}

impl Writer<'_> {
    pub fn set_account_config(&mut self, account: AccountId, c: &AccountConfig) -> Result<()> {
        self.tx.open_table(CONFIG)?.insert(account, enc(c).as_slice())?;
        Ok(())
    }

    pub fn set_reached(&mut self, account: AccountId, r: Reached) -> Result<()> {
        let mut t = self.tx.open_table(CONFIG)?;
        let Some(mut c) = t.get(account)?.map(|v| dec::<AccountConfig>(v.value())).transpose()? else { return Ok(()) };
        c.reached = r;
        t.insert(account, enc(&c).as_slice())?;
        Ok(())
    }

    /// Remove an account and its local mail (§5.4). Its Arca items are the
    /// caller's to delete.
    pub fn remove_account(&mut self, account: AccountId) -> Result<()> {
        let folders: Vec<(u32, crate::Folder)> = {
            let t = self.tx.open_table(crate::FOLDERS)?;
            let mut v = Vec::new();
            for e in t.iter()? {
                let (k, f) = e?;
                let f: crate::Folder = dec(f.value())?;
                if f.account == account {
                    v.push((k.value(), f));
                }
            }
            v
        };
        for (id, f) in &folders {
            self.clear_folder(*id)?;
            self.tx.open_table(crate::FOLDERS)?.remove(*id)?;
            self.tx.open_table(crate::FOLDER_NAMES)?.remove((account, f.name.as_str()))?;
        }
        self.clear_accepted_imports(account)?;
        self.tx.open_table(crate::ACCOUNTS)?.remove(account)?;
        self.tx.open_table(crate::IDENTITIES)?.remove(account)?;
        self.tx.open_table(CONFIG)?.remove(account)?;
        self.tx.open_table(crate::away::AWAY)?.remove(account)?;
        self.tx.open_table(crate::sieve::SIEVE)?.remove(account)?;
        for name in ["settings", "sync"] {
            let keys: Vec<String> = if name == "settings" {
                let t = self.tx.open_table(crate::senders::SETTINGS)?;
                let mut v = Vec::new();
                for e in t.range((account, "")..)? {
                    let (k, _) = e?;
                    if k.value().0 != account { break }
                    v.push(k.value().1.to_string());
                }
                v
            } else {
                let t = self.tx.open_table(crate::senders::SYNC_STATE)?;
                let mut v = Vec::new();
                for e in t.range((account, "")..)? {
                    let (k, _) = e?;
                    if k.value().0 != account { break }
                    v.push(k.value().1.to_string());
                }
                v
            };
            for k in keys {
                if name == "settings" {
                    self.tx.open_table(crate::senders::SETTINGS)?.remove((account, k.as_str()))?;
                } else {
                    self.tx.open_table(crate::senders::SYNC_STATE)?.remove((account, k.as_str()))?;
                }
            }
        }
        Ok(())
    }
}
