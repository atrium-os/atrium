//! Publisher records (opifex.md §5.2, build step C7).
//!
//! A publisher is a record in a catalog's log: a stable id (`pub:` plus 16 hex
//! of its first key's id), a display name, the domains it proved, the kinds it
//! may publish, and its current key. Four actions change it:
//!
//! - `register`: new publisher, signed by its own key (it holds the key).
//! - `rotate`: new key, signed by the current key and the new one.
//! - `scope`: new name, domains, kinds; signed by the current key.
//! - `recover`: new key after a loss, signed by the catalog's root-key
//!   threshold and the new key. Machines show it to people (§7.7).
//!
//! Domains are proved per machine (`_atrium-publisher.<domain>` TXT or
//! `https://<domain>/.well-known/atrium-publisher`); this module only says
//! which domains a record claims and what a proof must contain ([`proof_ok`]).

use serde::{Deserialize, Serialize};

use crate::{key_id, sign, verify, Root, KINDS};

pub const ACTIONS: [&str; 4] = ["register", "rotate", "scope", "recover"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PublisherEntry {
    pub id: String,
    pub action: String,
    pub name: String,
    pub domains: Vec<String>,
    pub kinds: Vec<String>,
    #[serde(default)]
    pub devices: bool,
    /// PEM of the key that is current after this entry.
    pub key: String,
    /// `<key id> <base64 signature>` over [`PublisherEntry::message`].
    pub sigs: Vec<String>,
}

/// A publisher as a machine knows it after replaying the log.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Publisher {
    pub id: String,
    pub name: String,
    pub domains: Vec<String>,
    pub prefixes: Vec<String>,
    pub kinds: Vec<String>,
    #[serde(default)]
    pub devices: bool,
    pub key: String,
    pub key_id: String,
    /// Keys it used before, oldest first.
    #[serde(default)]
    pub past: Vec<String>,
    /// How many recoveries (root-signed key replacements) it has had.
    #[serde(default)]
    pub recoveries: u64,
}

/// `pub:` plus the first 16 hex digits of the key id.
pub fn publisher_id(pem: &str) -> Result<String, String> {
    Ok(format!("pub:{}", &key_id(pem)?[..16]))
}

/// `example.com` → `com.example.`
pub fn prefix_of(domain: &str) -> String {
    let mut parts: Vec<&str> = domain.trim_end_matches('.').split('.').collect();
    parts.reverse();
    format!("{}.", parts.join("."))
}

fn domain_ok(d: &str) -> bool {
    let labels: Vec<&str> = d.split('.').collect();
    labels.len() >= 2
        && d.len() <= 253
        && labels.iter().all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
}

impl PublisherEntry {
    pub fn message(&self) -> Result<String, String> {
        Ok(format!("atrium-catalog publisher\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
                   self.id, self.action, self.name, self.domains.join(","), self.kinds.join(","),
                   self.devices, key_id(&self.key)?))
    }

    /// Ids of the keys whose signatures over the message verify against `pem_of`.
    fn signed_by(&self, pems: &[&str]) -> Result<Vec<String>, String> {
        let msg = self.message()?;
        let mut out = Vec::new();
        for line in &self.sigs {
            let mut it = line.split_whitespace();
            let (Some(id), Some(sig)) = (it.next(), it.next()) else { continue };
            if out.iter().any(|x| x == id) {
                continue;
            }
            if let Some(pem) = pems.iter().find(|p| key_id(p).ok().as_deref() == Some(id)) {
                if verify(msg.as_bytes(), sig, pem) {
                    out.push(id.to_string());
                }
            }
        }
        Ok(out)
    }

    fn shape(&self, third_party: bool) -> Result<(), String> {
        let what = format!("publisher {}", self.id);
        if !ACTIONS.contains(&self.action.as_str()) {
            return Err(format!("{what}: unknown action {:?}", self.action));
        }
        if self.name.trim().is_empty() || self.name.len() > 80 || self.name.chars().any(char::is_control) {
            return Err(format!("{what}: name"));
        }
        if self.domains.is_empty() || !self.domains.iter().all(|d| domain_ok(d)) {
            return Err(format!("{what}: domains must be lowercase host names"));
        }
        if self.kinds.is_empty() || !self.kinds.iter().all(|k| KINDS.contains(&k.as_str())) {
            return Err(format!("{what}: kinds"));
        }
        if third_party {
            if self.kinds.iter().any(|k| k != "app") || self.devices {
                return Err(format!("{what}: a third-party catalog carries apps only"));
            }
            if self.domains.iter().any(|d| prefix_of(d).starts_with("org.atrium.") || d == "atrium.org") {
                return Err(format!("{what}: org.atrium. is never accepted from a third-party catalog"));
            }
        }
        Ok(())
    }

    /// Apply this entry to the known publishers, checking its signatures.
    pub fn apply(&self, root: &Root, third_party: bool, known: &mut Vec<Publisher>) -> Result<(), String> {
        self.shape(third_party)?;
        let what = format!("publisher {}", self.id);
        let new_id = key_id(&self.key)?;
        let pos = known.iter().position(|p| p.id == self.id);
        let prefixes: Vec<String> = self.domains.iter().map(|d| prefix_of(d)).collect();
        for p in known.iter().filter(|p| p.id != self.id) {
            if p.key_id == new_id || p.past.contains(&new_id) {
                return Err(format!("{what}: key belongs to {}", p.id));
            }
            if let Some(d) = self.domains.iter().find(|d| p.domains.contains(d)) {
                return Err(format!("{what}: domain {d} belongs to {}", p.id));
            }
        }
        match (self.action.as_str(), pos) {
            ("register", None) => {
                if self.id != publisher_id(&self.key)? {
                    return Err(format!("{what}: id is not derived from its key"));
                }
                if self.signed_by(&[&self.key])?.len() != 1 {
                    return Err(format!("{what}: not signed by its key"));
                }
                known.push(Publisher { id: self.id.clone(), name: self.name.clone(), domains: self.domains.clone(),
                                       prefixes, kinds: self.kinds.clone(), devices: self.devices,
                                       key: self.key.clone(), key_id: new_id, past: Vec::new(), recoveries: 0 });
            }
            ("register", Some(_)) => return Err(format!("{what}: registered already")),
            (_, None) => return Err(format!("{what}: not registered")),
            (action, Some(i)) => {
                let cur = known[i].clone();
                let rekey = new_id != cur.key_id;
                if rekey && (cur.past.contains(&new_id)) {
                    return Err(format!("{what}: a past key can't come back"));
                }
                match action {
                    "scope" => {
                        if rekey {
                            return Err(format!("{what}: scope keeps the key (rotate first)"));
                        }
                        if self.signed_by(&[&cur.key])?.len() != 1 {
                            return Err(format!("{what}: scope change not signed by its key"));
                        }
                    }
                    "rotate" => {
                        if !rekey {
                            return Err(format!("{what}: rotate to the same key"));
                        }
                        if self.signed_by(&[&cur.key, &self.key])?.len() != 2 {
                            return Err(format!("{what}: rotation needs the old and the new key"));
                        }
                    }
                    _ => {
                        // recover
                        let roots: Vec<&str> = root.with_role("root").map(|k| k.pem.as_str()).collect();
                        if root.threshold == 0 || self.signed_by(&roots)?.len() < root.threshold {
                            return Err(format!("{what}: recovery needs {} root keys", root.threshold));
                        }
                        if self.signed_by(&[&self.key])?.len() != 1 {
                            return Err(format!("{what}: recovery not signed by the new key"));
                        }
                    }
                }
                let p = &mut known[i];
                if rekey {
                    p.past.push(cur.key_id.clone());
                    p.key = self.key.clone();
                    p.key_id = new_id;
                }
                if action == "recover" {
                    p.recoveries += 1;
                }
                p.name = self.name.clone();
                p.domains = self.domains.clone();
                p.prefixes = prefixes;
                p.kinds = self.kinds.clone();
                p.devices = self.devices;
            }
        }
        Ok(())
    }

    /// Add one signature line.
    pub fn sign_with(&mut self, key_pem: &str, pub_pem: &str) -> Result<(), String> {
        let line = format!("{} {}", key_id(pub_pem)?, sign(self.message()?.as_bytes(), key_pem)?);
        self.sigs.push(line);
        Ok(())
    }
}

/// What a domain proof must contain: a line `atrium-publisher <publisher id>`.
/// The TXT record and the `.well-known` file use the same text.
pub fn proof_text(id: &str) -> String { format!("atrium-publisher {id}\n") }

/// Does a fetched proof (TXT strings or the file's text) name this publisher?
pub fn proof_ok(text: &str, id: &str) -> bool {
    text.lines().map(str::trim).any(|l| l.trim_matches('"') == format!("atrium-publisher {id}"))
}

/// Where a machine looks for a domain's proof.
pub fn proof_url(domain: &str) -> String { format!("https://{domain}/.well-known/atrium-publisher") }
pub fn proof_txt_name(domain: &str) -> String { format!("_atrium-publisher.{domain}") }

impl PublisherEntry {
    /// The log entry carrying this record.
    pub fn entry(self, now: u64) -> crate::Entry {
        crate::Entry { kind: "publisher".into(), time: now, release: None, yank: None, locations: None, publisher: Some(self) }
    }
}
