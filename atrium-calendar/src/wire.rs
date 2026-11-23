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
    let payload = serde_json::to_vec(value)?;
    let mut frame = Vec::with_capacity(envelope::HEADER_LEN + payload.len());
    envelope::write_message(
        &mut frame,
        Header::new(CLASS_CALENDAR, OP_V1, flags, payload.len() as u32),
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

fn receive<T: DeserializeOwned>(s: &mut UnixStream, flags: u16, limit: usize) -> io::Result<T> {
    let deadline = Instant::now() + WAIT;
    let mut bytes = [0; envelope::HEADER_LEN];
    read_until(s, &mut bytes, deadline)?;
    let h = Header::decode(&bytes)?;
    if h.opcode_class != CLASS_CALENDAR
        || h.op != OP_V1
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
    serde_json::from_slice(&payload).map_err(Into::into)
}

pub fn call(socket: &Path, request: &Request) -> Result<Day, String> {
    exchange(socket, request)
}

pub fn page(socket: &Path, request: &Request) -> Result<CalendarPage, String> {
    let page: CalendarPage = exchange(socket, request)?;
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

fn exchange<T: DeserializeOwned>(socket: &Path, request: &Request) -> Result<T, String> {
    let mut s =
        UnixStream::connect(socket).map_err(|e| format!("Calendar service unavailable: {e}"))?;
    s.set_nonblocking(true).map_err(|e| e.to_string())?;
    send(&mut s, flag::RESPONSE_EXPECTED, request).map_err(|e| e.to_string())?;
    receive(&mut s, flag::IS_RESPONSE, 1024 * 1024)
        .map_err(|e| format!("Refresh to check the saved state: {e}"))?
}

pub fn serve(s: &mut UnixStream, store: &mut Store) -> io::Result<()> {
    s.set_nonblocking(true)?;
    let reply: Result<CalendarPage, String> = match receive(s, flag::RESPONSE_EXPECTED, 4096) {
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
        Ok(Request::Add { revision, entry }) => {
            let date = entry.date;
            store
                .add(revision, entry)
                .map(|_| store.page(date, Some(CalendarView::Today)))
        }
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
                .map_or(shown, |edit| edit.date);
            store
                .change(revision, id, shown, scope, replacement)
                .map(|()| store.page(date, Some(CalendarView::Today)))
        }
        Ok(Request::Release { revision, date, id }) => store
            .release(revision, date, id)
            .map(|()| store.page(date, Some(CalendarView::Today))),
        Err(_) => Err("Invalid Calendar request".into()),
    };
    send(s, flag::IS_RESPONSE, &reply)
}
