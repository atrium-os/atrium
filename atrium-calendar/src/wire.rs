//! One bounded request and reply per connection. Version 1 uses inline JSON.
use super::{
    store::Store, CalendarPage, CalendarView, Day, EditScope, Entry, EventEdit, SearchPage,
    SearchQuery,
};
use aqueduct::{classes::CLASS_CALENDAR, envelope, flag, Header};
use chrono::NaiveDate;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::net::UnixStream,
    },
    path::Path,
    time::{Duration, Instant},
};

const OP_V1: u16 = 1;
const OP_PREPARE_IMPORT: u16 = 2;
const WAIT: Duration = Duration::from_secs(2);
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum EventChange {
    Update(EventEdit),
    Delete,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Request {
    ReviewInvitation {
        revision: u64,
        source: portcullis_ipc::calendar::Source,
        proposal: crate::invitation::Proposal,
    },
    ApplyInvitation {
        revision: u64,
        token: uuid::Uuid,
    },
    CheckInvitation {
        revision: u64,
        token: uuid::Uuid,
    },

    PrepareImport {
        revision: u64,
        hash: [u8; 32],
        rows: Vec<Entry>,
    },
    SaveImport {
        revision: u64,
        token: uuid::Uuid,
    },
    DiscardImport {
        token: uuid::Uuid,
    },
    Undo {
        receipt: super::UndoReceipt,
    },
    Today {
        view: Option<CalendarView>,
    },
    SetViewingZone {
        revision: u64,
        pinned: Option<chrono_tz::Tz>,
        date: NaiveDate,
        view: CalendarView,
    },
    Search {
        query: SearchQuery,
        offset: usize,
        revision: Option<u64>,
    },
    OpenSearchHit {
        revision: u64,
        id: uuid::Uuid,
        date: NaiveDate,
    },
    Browse {
        date: NaiveDate,
        view: Option<CalendarView>,
    },
    SetOpeningView {
        revision: u64,
        opening_view: CalendarView,
        date: NaiveDate,
        view: CalendarView,
    },
    Day {
        date: NaiveDate,
    },
    Add {
        revision: u64,
        entry: Entry,
    },
    Change {
        revision: u64,
        id: uuid::Uuid,
        shown: NaiveDate,
        scope: EditScope,
        operation: EventChange,
    },
    Release {
        revision: u64,
        date: NaiveDate,
        id: uuid::Uuid,
    },
}

// A single deadline bounds fragmented frames, including partial writes to a slow peer.
fn ready(fd: RawFd, events: libc::c_short, deadline: Instant) -> io::Result<()> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        let mut p = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: the poll array contains exactly one initialized descriptor.
        let n = unsafe { libc::poll(&mut p, 1, left.as_millis().max(1) as i32) };
        if n > 0 {
            return Ok(());
        }
        if n == 0 {
            continue;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

fn send<T: Serialize>(s: &mut UnixStream, flags: u16, value: &T) -> io::Result<()> {
    send_op(s, flags, OP_V1, value)
}
fn send_op<T: Serialize>(s: &mut UnixStream, flags: u16, op: u16, value: &T) -> io::Result<()> {
    let payload = serde_json::to_vec(value)?;
    let mut frame = Vec::with_capacity(envelope::HEADER_LEN + payload.len());
    envelope::write_message(
        &mut frame,
        Header::new(CLASS_CALENDAR, op, flags, payload.len() as u32),
        &payload,
    )?;
    let deadline = Instant::now() + WAIT;
    let mut bytes = frame.as_slice();
    while !bytes.is_empty() {
        ready(s.as_raw_fd(), libc::POLLOUT, deadline)?;
        match s.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn read_until(s: &mut UnixStream, mut bytes: &mut [u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        ready(s.as_raw_fd(), libc::POLLIN, deadline)?;
        match s.read(bytes) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => bytes = &mut bytes[n..],
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn read_frame<T: DeserializeOwned>(
    s: &mut UnixStream,
    flags: u16,
    limit: usize,
    expected_op: Option<u16>,
) -> io::Result<(u16, T)> {
    let deadline = Instant::now() + WAIT;
    let mut bytes = [0; envelope::HEADER_LEN];
    read_until(s, &mut bytes, deadline)?;
    let h = Header::decode(&bytes)?;
    let import = flags == flag::RESPONSE_EXPECTED && h.op == OP_PREPARE_IMPORT;
    let limit = if import {
        crate::import::FRAME_LIMIT
    } else {
        limit
    };
    if h.opcode_class != CLASS_CALENDAR
        || !matches!(h.op, OP_V1 | OP_PREPARE_IMPORT)
        || expected_op.is_some_and(|op| h.op != op)
        || h.flags != flags
        || h.length as usize > limit
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid Calendar envelope",
        ));
    }
    let mut payload = vec![0; h.length as usize];
    read_until(s, &mut payload, deadline)?;
    Ok((h.op, serde_json::from_slice(&payload)?))
}

pub fn call(socket: &Path, request: &Request) -> Result<Day, String> {
    exchange(socket, request)
}

pub fn page(socket: &Path, request: &Request) -> Result<CalendarPage, String> {
    let page: CalendarPage = exchange(socket, request)?;
    if let Some(receipt) = page.undo {
        let revision = match request {
            Request::ApplyInvitation { revision, .. }
            | Request::SaveImport { revision, .. }
            | Request::Add { revision, .. }
            | Request::Release { revision, .. }
            | Request::Change { revision, .. } => *revision,
            _ => return Err("Unexpected undo receipt".into()),
        };
        if receipt.token.is_nil()
            || receipt.revision != page.revision
            || revision.checked_add(1) != Some(page.revision)
        {
            return Err("Invalid undo receipt".into());
        }
    }
    if page.entries.iter().any(|entry| entry.validate().is_err()) {
        return Err("Invalid Calendar reply".into());
    }
    Ok(page)
}

pub fn search(
    socket: &Path,
    query: SearchQuery,
    offset: usize,
    revision: Option<u64>,
) -> Result<SearchPage, String> {
    let (start, end) = (query.start, query.end);
    let result: SearchPage = exchange(
        socket,
        &Request::Search {
            query,
            offset,
            revision,
        },
    )?;
    result.validate(start, end, offset, revision)?;
    Ok(result)
}

pub fn prepare_import(
    socket: &Path,
    revision: u64,
    hash: [u8; 32],
    rows: Vec<Entry>,
) -> Result<crate::import::Prepared, String> {
    exchange(
        socket,
        &Request::PrepareImport {
            revision,
            hash,
            rows,
        },
    )
}
pub fn discard_import(socket: &Path, token: uuid::Uuid) -> Result<(), String> {
    exchange(socket, &Request::DiscardImport { token })
}

pub fn exchange<T: DeserializeOwned>(socket: &Path, request: &Request) -> Result<T, String> {
    let mut s =
        UnixStream::connect(socket).map_err(|e| format!("Calendar service unavailable: {e}"))?;
    s.set_nonblocking(true).map_err(|e| e.to_string())?;
    let op = if matches!(
        request,
        Request::PrepareImport { .. } | Request::ReviewInvitation { .. }
    ) {
        OP_PREPARE_IMPORT
    } else {
        OP_V1
    };
    send_op(&mut s, flag::RESPONSE_EXPECTED, op, request).map_err(|e| e.to_string())?;
    read_frame(&mut s, flag::IS_RESPONSE, 1024 * 1024, Some(op))
        .map(|(_, value)| value)
        .map_err(|e| format!("Refresh to check the saved state: {e}"))?
}

pub fn refuse(s: &mut UnixStream, reason: &str) -> io::Result<()> {
    s.set_nonblocking(true)?;
    let (op, _): (u16, Request) = read_frame(s, flag::RESPONSE_EXPECTED, 4096, None)?;
    send_op(
        s,
        flag::IS_RESPONSE,
        op,
        &Result::<CalendarPage, _>::Err(reason),
    )
}

pub fn serve(s: &mut UnixStream, store: &mut Store) -> io::Result<()> {
    s.set_nonblocking(true)?;
    let reply: Result<CalendarPage, String> =
        match read_frame(s, flag::RESPONSE_EXPECTED, 4096, None).and_then(|(op, req)| {
            if (op == OP_PREPARE_IMPORT)
                != matches!(
                    req,
                    Request::PrepareImport { .. } | Request::ReviewInvitation { .. }
                )
            {
                return Err(io::ErrorKind::InvalidData.into());
            }
            Ok(req)
        }) {
            Ok(Request::ReviewInvitation {
                revision,
                source,
                proposal,
            }) => {
                return send_op(
                    s,
                    flag::IS_RESPONSE,
                    OP_PREPARE_IMPORT,
                    &store.review_invitation(revision, &source, proposal),
                );
            }
            Ok(Request::CheckInvitation { revision, token }) => {
                return send(
                    s,
                    flag::IS_RESPONSE,
                    &store.check_invitation(revision, token),
                );
            }
            Ok(Request::ApplyInvitation { revision, token }) => store
                .apply_invitation(revision, token)
                .map(|(date, receipt)| mutation_page(store, date, Some(receipt))),
            Ok(Request::PrepareImport {
                revision,
                hash,
                rows,
            }) => {
                return send_op(
                    s,
                    flag::IS_RESPONSE,
                    OP_PREPARE_IMPORT,
                    &store.prepare_import(revision, hash, rows),
                )
            }
            Ok(Request::DiscardImport { token }) => {
                store.discard_import(token);
                return send(s, flag::IS_RESPONSE, &Result::<(), String>::Ok(()));
            }
            Ok(Request::SaveImport { revision, token }) => store
                .save_import(revision, token)
                .map(|(date, receipt)| mutation_page(store, date, Some(receipt))),
            Ok(Request::Search {
                query,
                offset,
                revision,
            }) => {
                return send(
                    s,
                    flag::IS_RESPONSE,
                    &store.search(&query, offset, revision),
                );
            }
            Ok(Request::OpenSearchHit { revision, id, date }) => {
                store.open_search_hit(revision, id, date)
            }
            Ok(Request::Undo { receipt }) => store
                .undo(receipt)
                .map(|date| store.page(date, Some(CalendarView::Today))),
            Ok(Request::Today { view }) => Ok(store.page(
                chrono::Utc::now()
                    .with_timezone(&store.viewing_zone())
                    .date_naive(),
                view,
            )),
            Ok(Request::SetViewingZone {
                revision,
                pinned,
                date,
                view,
            }) => {
                let now = chrono::Utc::now();
                let today = date == now.with_timezone(&store.viewing_zone()).date_naive();
                store.set_viewing_zone(revision, pinned).map(|()| {
                    let anchor = if today {
                        now.with_timezone(&store.viewing_zone()).date_naive()
                    } else {
                        date
                    };
                    store.page(anchor, Some(view))
                })
            }
            Ok(Request::Browse { date, view }) => Ok(store.page(date, view)),
            Ok(Request::SetOpeningView {
                revision,
                opening_view,
                date,
                view,
            }) => store
                .set_opening_view(revision, opening_view)
                .map(|()| store.page(date, Some(view))),
            Ok(Request::Day { date }) => Ok(store.page(date, Some(CalendarView::Today))),
            Ok(Request::Add { revision, entry }) => entry
                .fields_on(entry.date)
                .agenda_date(store.viewing_zone())
                .map_err(str::to_owned)
                .and_then(|date| {
                    store
                        .add(revision, entry)
                        .map(|receipt| mutation_page(store, date, Some(receipt)))
                }),
            Ok(Request::Change {
                revision,
                id,
                shown,
                scope,
                operation,
            }) => {
                let replacement = match operation {
                    EventChange::Update(fields) => Some(fields),
                    EventChange::Delete => None,
                };
                let date = replacement
                    .as_ref()
                    .filter(|_| scope == EditScope::ThisEvent)
                    .map_or(Ok(shown), |edit| edit.agenda_date(store.viewing_zone()));
                date.map_err(str::to_owned).and_then(|date| {
                    store
                        .change(revision, id, shown, scope, replacement)
                        .map(|receipt| mutation_page(store, date, receipt))
                })
            }
            Ok(Request::Release { revision, date, id }) => store
                .release(revision, date, id)
                .map(|receipt| mutation_page(store, date, Some(receipt))),
            Err(_) => Err("Invalid Calendar request".into()),
        };
    send(s, flag::IS_RESPONSE, &reply)
}

fn mutation_page(store: &Store, date: NaiveDate, undo: Option<super::UndoReceipt>) -> CalendarPage {
    CalendarPage {
        undo,
        ..store.page(date, Some(CalendarView::Today))
    }
}
