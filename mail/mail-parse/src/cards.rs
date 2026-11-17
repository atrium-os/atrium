//! Cards (mail.md §14.3): an order, delivery, trip or booking the sender
//! described in schema.org JSON-LD inside its HTML, read into a few fixed
//! lines. Nothing in it is fetched or followed (no URLs are kept); the
//! engine shows it only when the sender is verified.

use mail_schema::{Card, CardField as F, CardKind as K, CARD_FIELDS_MAX, CARD_VALUE_MAX};
use serde_json::Value;

/// Most JSON-LD blocks read, and their largest size.
const BLOCKS_MAX: usize = 8;
const BLOCK_MAX: usize = 256 << 10;

/// The JSON-LD blocks of an HTML document.
fn blocks(html: &str) -> Vec<&str> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut at = 0;
    while out.len() < BLOCKS_MAX {
        let Some(i) = lower[at..].find("<script") else { break };
        let start = at + i;
        let Some(gt) = lower[start..].find('>') else { break };
        let tag = &lower[start..start + gt];
        let body = start + gt + 1;
        let Some(end) = lower[body..].find("</script") else { break };
        if tag.contains("application/ld+json") && end <= BLOCK_MAX {
            out.push(&html[body..body + end]);
        }
        at = body + end;
    }
    out
}

/// The card for an HTML body, if it describes something Mail knows.
pub fn card(html: &str) -> Option<Card> {
    let parsed: Vec<Value> = blocks(html).into_iter().filter_map(|b| serde_json::from_str::<Value>(b.trim()).ok()).collect();
    let mut items = Vec::new();
    for v in &parsed {
        flatten(v, &mut items, 0);
    }
    let n = items.len();
    let mut found = items.into_iter().filter_map(one).collect::<Vec<_>>();
    if found.is_empty() {
        return None;
    }
    let mut c = found.remove(0);
    // More legs of the same trip (or more parcels): said in the title.
    let more = found.iter().filter(|o| o.kind == c.kind).count();
    if more > 0 && n > 1 {
        c.title = clip(&format!("{} (and {more} more)", c.title));
    }
    Some(c)
}

/// The top-level things: a list, an `@graph`, or one object.
fn flatten<'a>(v: &'a Value, out: &mut Vec<&'a Value>, depth: u8) {
    if depth > 2 || out.len() >= 32 {
        return;
    }
    match v {
        Value::Array(a) => a.iter().for_each(|x| flatten(x, out, depth + 1)),
        Value::Object(o) => match o.get("@graph") {
            Some(g) => flatten(g, out, depth + 1),
            None => out.push(v),
        },
        _ => {}
    }
}

/// Plain text: no controls, spaces collapsed, at most `CARD_VALUE_MAX` bytes.
fn clip(s: &str) -> String {
    let t: String = s.split(|c: char| c.is_whitespace() || c.is_control()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for ch in t.chars() {
        if out.len() + ch.len_utf8() > CARD_VALUE_MAX {
            break;
        }
        out.push(ch);
    }
    out
}

fn type_of(v: &Value) -> String {
    let t = match v.get("@type") {
        Some(Value::String(s)) => s.as_str(),
        Some(Value::Array(a)) => a.iter().find_map(Value::as_str).unwrap_or(""),
        _ => "",
    };
    t.rsplit(['/', ':']).next().unwrap_or("").to_string()
}

/// A text value: a string, a number, or a thing's `name`.
fn text(v: Option<&Value>) -> Option<String> {
    let s = match v? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Object(o) => return text(o.get("name")),
        Value::Array(a) => return text(a.first()),
        _ => return None,
    };
    // A URL is never shown: cards keep no links.
    if s.contains("://") {
        return None;
    }
    let c = clip(&s);
    (!c.is_empty()).then_some(c)
}

fn get<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut v = v;
    for k in path {
        v = match v {
            Value::Array(a) => a.first()?.get(*k)?,
            _ => v.get(*k)?,
        };
    }
    Some(v)
}

fn t(v: &Value, path: &[&str]) -> Option<String> {
    text(get(v, path))
}

/// A schema.org enumeration value (`http://schema.org/OrderDelivered`) in words.
fn status(v: &Value, key: &str, prefix: &str) -> Option<String> {
    let s = match v.get(key)? {
        Value::String(s) => s.clone(),
        Value::Object(o) => o.get("name").or(o.get("@id")).and_then(Value::as_str)?.to_string(),
        _ => return None,
    };
    let last = s.rsplit(['/', ':']).next().unwrap_or(&s);
    let last = last.strip_prefix(prefix).unwrap_or(last);
    // "InTransit" → "In transit".
    let mut out = String::new();
    for (i, ch) in last.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            out.push(' ');
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    let c = clip(&out);
    (!c.is_empty()).then_some(c)
}

/// `2027-03-14T09:05:00+05:30` → "14 Mar 2027, 09:05" (the time as the
/// sender wrote it: a departure is in its airport's time).
fn when(v: &Value, keys: &[&str]) -> Option<String> {
    let s = keys.iter().find_map(|k| v.get(*k).and_then(Value::as_str))?;
    Some(format_time(s).unwrap_or_else(|| clip(s)))
}

pub fn format_time(s: &str) -> Option<String> {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let s = s.trim();
    let date = s.get(..10)?;
    let mut d = date.split('-');
    let (y, m, day): (u32, usize, u32) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || !(1900..=2200).contains(&y) {
        return None;
    }
    let mut out = format!("{day} {} {y}", MONTHS[m - 1]);
    if let Some(rest) = s.get(10..).and_then(|r| r.strip_prefix(['T', ' '])) {
        let hm = rest.get(..5)?;
        let (h, mi) = hm.split_once(':')?;
        let (h, mi): (u32, u32) = (h.parse().ok()?, mi.parse().ok()?);
        if h > 23 || mi > 59 {
            return None;
        }
        out += &format!(", {h:02}:{mi:02}");
    }
    Some(out)
}

fn address(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => text(Some(&Value::String(s.clone()))),
        Value::Object(o) => {
            let parts: Vec<String> = ["streetAddress", "addressLocality", "postalCode", "addressRegion", "addressCountry"].iter().filter_map(|k| text(o.get(*k))).collect();
            let c = clip(&parts.join(", "));
            (!c.is_empty()).then_some(c)
        }
        _ => None,
    }
}

fn price(v: &Value) -> Option<String> {
    let amount = ["totalPrice", "price"].iter().find_map(|k| match v.get(*k)? {
        Value::Object(o) => text(o.get("price")).map(|p| (p, text(o.get("priceCurrency")))),
        x => text(Some(x)).map(|p| (p, None)),
    });
    let (p, cur) = amount?;
    let cur = cur.or_else(|| t(v, &["priceCurrency"]));
    Some(clip(&match cur {
        Some(c) => format!("{p} {c}"),
        None => p,
    }))
}

struct Lines(Vec<(F, String)>);

impl Lines {
    fn add(&mut self, f: F, v: Option<String>) -> &mut Self {
        if let Some(v) = v {
            if self.0.len() < CARD_FIELDS_MAX && !self.0.iter().any(|(g, w)| *g == f && *w == v) {
                self.0.push((f, v));
            }
        }
        self
    }
}

fn one(v: &Value) -> Option<Card> {
    let ty = type_of(v);
    let mut l = Lines(Vec::new());
    let (kind, title) = match ty.as_str() {
        "Order" => {
            let n = t(v, &["orderNumber"]);
            l.add(F::Status, status(v, "orderStatus", "Order"))
                .add(F::Merchant, t(v, &["merchant"]).or_else(|| t(v, &["seller"])))
                .add(F::Number, n.clone())
                .add(F::Total, price(v).or_else(|| get(v, &["acceptedOffer"]).and_then(price)));
            let offers: Vec<&Value> = match v.get("acceptedOffer") {
                Some(Value::Array(a)) => a.iter().collect(),
                Some(o) => vec![o],
                None => Vec::new(),
            };
            for o in offers.iter().take(3) {
                l.add(F::Item, t(o, &["itemOffered", "name"]));
            }
            if let Some(d) = v.get("orderDelivery") {
                l.add(F::Carrier, t(d, &["carrier"]).or_else(|| t(d, &["provider"]))).add(F::Tracking, t(d, &["trackingNumber"])).add(F::Delivery, when(d, &["expectedArrivalUntil", "expectedArrivalFrom"]));
            }
            (K::Order, n.map(|n| format!("Order {n}")).unwrap_or_else(|| "Order".into()))
        }
        "ParcelDelivery" => {
            l.add(F::Status, status(v, "deliveryStatus", "Order"))
                .add(F::Carrier, t(v, &["carrier"]).or_else(|| t(v, &["provider"])))
                .add(F::Tracking, t(v, &["trackingNumber"]))
                .add(F::Delivery, when(v, &["expectedArrivalUntil", "expectedArrivalFrom"]))
                .add(F::Item, t(v, &["itemShipped", "name"]))
                .add(F::Number, t(v, &["partOfOrder", "orderNumber"]))
                .add(F::Merchant, t(v, &["partOfOrder", "merchant"]));
            (K::Parcel, t(v, &["itemShipped", "name"]).map(|i| format!("Delivery: {i}")).unwrap_or_else(|| "Delivery".into()))
        }
        "FlightReservation" => {
            let f = v.get("reservationFor")?;
            let flight = match (t(f, &["airline", "iataCode"]), t(f, &["flightNumber"])) {
                (Some(a), Some(n)) if !n.starts_with(&a) => Some(format!("{a} {n}")),
                (_, Some(n)) => Some(n),
                (a, None) => a.and(None),
            };
            let from = t(f, &["departureAirport", "iataCode"]).or_else(|| t(f, &["departureAirport"]));
            let to = t(f, &["arrivalAirport", "iataCode"]).or_else(|| t(f, &["arrivalAirport"]));
            l.add(F::Status, status(v, "reservationStatus", "Reservation"))
                .add(F::Flight, flight.clone())
                .add(F::From, t(f, &["departureAirport"]).or(from.clone()))
                .add(F::Departs, when(f, &["departureTime"]))
                .add(F::To, t(f, &["arrivalAirport"]).or(to.clone()))
                .add(F::Arrives, when(f, &["arrivalTime"]))
                .add(F::Passenger, t(v, &["underName"]))
                .add(F::Seat, t(v, &["airplaneSeat"]).or_else(|| t(v, &["reservedTicket", "ticketedSeat", "seatNumber"])))
                .add(F::Number, t(v, &["reservationNumber"]));
            let route = match (from, to) {
                (Some(a), Some(b)) => format!(" {a} → {b}"),
                _ => String::new(),
            };
            (K::Flight, format!("{}{route}", flight.unwrap_or_else(|| "Flight".into())))
        }
        "TrainReservation" | "BusReservation" => {
            let f = v.get("reservationFor")?;
            let train = ty == "TrainReservation";
            let (dep, arr, num) = if train { ("departureStation", "arrivalStation", "trainNumber") } else { ("departureBusStop", "arrivalBusStop", "busNumber") };
            let (from, to) = (t(f, &[dep]), t(f, &[arr]));
            l.add(F::Status, status(v, "reservationStatus", "Reservation"))
                .add(F::Number, t(f, &[num]))
                .add(F::From, from.clone())
                .add(F::Departs, when(f, &["departureTime"]))
                .add(F::To, to.clone())
                .add(F::Arrives, when(f, &["arrivalTime"]))
                .add(F::Seat, t(v, &["reservedTicket", "ticketedSeat", "seatNumber"]))
                .add(F::Passenger, t(v, &["underName"]));
            let what = if train { "Train" } else { "Bus" };
            let title = match (from, to) {
                (Some(a), Some(b)) => format!("{what}: {a} → {b}"),
                _ => what.to_string(),
            };
            (if train { K::Train } else { K::Bus }, title)
        }
        "LodgingReservation" => {
            let f = v.get("reservationFor")?;
            l.add(F::Status, status(v, "reservationStatus", "Reservation"))
                .add(F::Place, t(f, &["name"]))
                .add(F::Address, address(f.get("address")))
                .add(F::CheckIn, when(v, &["checkinTime", "checkinDate"]))
                .add(F::CheckOut, when(v, &["checkoutTime", "checkoutDate"]))
                .add(F::Guests, t(v, &["numAdults"]))
                .add(F::Number, t(v, &["reservationNumber"]));
            (K::Hotel, t(f, &["name"]).unwrap_or_else(|| "Stay".into()))
        }
        "FoodEstablishmentReservation" => {
            let f = v.get("reservationFor")?;
            l.add(F::Status, status(v, "reservationStatus", "Reservation"))
                .add(F::Place, t(f, &["name"]))
                .add(F::Address, address(f.get("address")))
                .add(F::Starts, when(v, &["startTime", "startDate"]))
                .add(F::Guests, t(v, &["partySize"]))
                .add(F::Number, t(v, &["reservationNumber"]));
            (K::Table, t(f, &["name"]).map(|n| format!("Table at {n}")).unwrap_or_else(|| "Table".into()))
        }
        "EventReservation" => {
            let f = v.get("reservationFor")?;
            l.add(F::Status, status(v, "reservationStatus", "Reservation"))
                .add(F::Starts, when(f, &["startDate", "startTime"]))
                .add(F::Place, t(f, &["location", "name"]).or_else(|| t(f, &["location"])))
                .add(F::Address, address(get(f, &["location", "address"])))
                .add(F::Seat, t(v, &["reservedTicket", "ticketedSeat", "seatNumber"]))
                .add(F::Number, t(v, &["reservationNumber"]));
            (K::Event, t(f, &["name"]).unwrap_or_else(|| "Event".into()))
        }
        "RentalCarReservation" => {
            let f = v.get("reservationFor")?;
            l.add(F::Status, status(v, "reservationStatus", "Reservation"))
                .add(F::Item, t(f, &["name"]).or_else(|| t(f, &["brand"])))
                .add(F::PickUp, [t(v, &["pickupLocation", "name"]).or_else(|| t(v, &["pickupLocation"])), when(v, &["pickupTime"])].into_iter().flatten().reduce(|a, b| format!("{a}, {b}")))
                .add(F::DropOff, [t(v, &["dropoffLocation", "name"]).or_else(|| t(v, &["dropoffLocation"])), when(v, &["dropoffTime"])].into_iter().flatten().reduce(|a, b| format!("{a}, {b}")))
                .add(F::Number, t(v, &["reservationNumber"]));
            (K::Car, t(v, &["provider"]).map(|p| format!("Car from {p}")).unwrap_or_else(|| "Car".into()))
        }
        _ => return None,
    };
    // A card with nothing on it says nothing.
    if l.0.is_empty() {
        return None;
    }
    let fields = l.0.into_iter().map(|(f, v)| (f, clip(&v))).filter(|(_, v)| !v.is_empty()).collect();
    Some(Card { kind, title: clip(&title), fields })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(format_time("2027-03-14T09:05:00+05:30").as_deref(), Some("14 Mar 2027, 09:05"));
        assert_eq!(format_time("2027-03-14").as_deref(), Some("14 Mar 2027"));
        assert_eq!(format_time("2027-13-14"), None);
        assert_eq!(format_time("soon"), None);
    }

    #[test]
    fn flight_order_hotel_and_nothing() {
        let flight = r#"<html><head><script type="application/ld+json">
        [{"@context":"http://schema.org","@type":"FlightReservation","reservationNumber":"RXJ34P",
          "reservationStatus":"http://schema.org/ReservationConfirmed",
          "underName":{"@type":"Person","name":"Priya Raman"},
          "reservationFor":{"@type":"Flight","flightNumber":"101","airline":{"name":"Air India","iataCode":"AI"},
            "departureAirport":{"name":"Indira Gandhi International","iataCode":"DEL"},"departureTime":"2027-03-14T09:05:00+05:30",
            "arrivalAirport":{"name":"Chhatrapati Shivaji","iataCode":"BOM"},"arrivalTime":"2027-03-14T11:20:00+05:30"},
          "airplaneSeat":"12A","url":"https://evil.example/track"},
         {"@type":"FlightReservation","reservationFor":{"@type":"Flight","flightNumber":"AI102",
            "departureAirport":{"iataCode":"BOM"},"arrivalAirport":{"iataCode":"DEL"}}}]
        </script></head><body>Your trip</body></html>"#;
        let c = card(flight).unwrap();
        assert_eq!(c.kind, K::Flight);
        assert_eq!(c.title, "AI 101 DEL → BOM (and 1 more)");
        assert!(c.fields.contains(&(F::Status, "Confirmed".into())));
        assert!(c.fields.contains(&(F::Departs, "14 Mar 2027, 09:05".into())));
        assert!(c.fields.contains(&(F::From, "Indira Gandhi International".into())));
        assert!(c.fields.contains(&(F::Seat, "12A".into())));
        assert!(c.fields.contains(&(F::Passenger, "Priya Raman".into())));
        assert!(!c.fields.iter().any(|(_, v)| v.contains("://")));

        let order = r#"<script TYPE='application/ld+json'>{"@context":"https://schema.org","@graph":[{"@type":"Order",
          "merchant":{"@type":"Organization","name":"Shop\n  Example"},"orderNumber":"1234-5678",
          "orderStatus":"https://schema.org/OrderInTransit","priceCurrency":"INR","price":"2,499.00",
          "acceptedOffer":[{"itemOffered":{"name":"Kettle"}},{"itemOffered":{"name":"Mug"}}],
          "orderDelivery":{"@type":"ParcelDelivery","carrier":{"name":"Blue Dart"},"trackingNumber":"BD123","expectedArrivalUntil":"2027-03-20"}}]}</script>"#;
        let c = card(order).unwrap();
        assert_eq!((c.kind, c.title.as_str()), (K::Order, "Order 1234-5678"));
        assert_eq!(c.fields, vec![
            (F::Status, "In transit".into()), (F::Merchant, "Shop Example".into()), (F::Number, "1234-5678".into()), (F::Total, "2,499.00 INR".into()),
            (F::Item, "Kettle".into()), (F::Item, "Mug".into()), (F::Carrier, "Blue Dart".into()), (F::Tracking, "BD123".into()), (F::Delivery, "20 Mar 2027".into()),
        ]);

        let hotel = r#"<script type="application/ld+json">{"@type":"LodgingReservation","reservationNumber":"H-9",
          "reservationFor":{"@type":"LodgingBusiness","name":"Lake View","address":{"streetAddress":"1 Lake Rd","addressLocality":"Udaipur","addressCountry":{"name":"India"}}},
          "checkinDate":"2027-04-01T14:00:00","checkoutDate":"2027-04-03"}</script>"#;
        let c = card(hotel).unwrap();
        assert_eq!(c.title, "Lake View");
        assert!(c.fields.contains(&(F::Address, "1 Lake Rd, Udaipur, India".into())));
        assert!(c.fields.contains(&(F::CheckIn, "1 Apr 2027, 14:00".into())));

        // Not a kind Mail knows, broken JSON, scripts that aren't JSON-LD: no card.
        assert!(card(r#"<script type="application/ld+json">{"@type":"Organization","name":"X"}</script>"#).is_none());
        assert!(card(r#"<script type="application/ld+json">{"@type":"Order",</script>"#).is_none());
        assert!(card(r#"<script>var x = {"@type":"Order","orderNumber":"1"}</script>"#).is_none());
        assert!(card("<p>hello</p>").is_none());
    }
}
