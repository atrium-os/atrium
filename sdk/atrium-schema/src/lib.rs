//! C from an atrium-message schema (app-composition.md §5.2, A5).
//!
//! `generate(schema_json, prefix)` returns a header and a source file that
//! define the schema's types as C structs and, for the root message,
//!
//! ```c
//! int  <root>_encode(const struct <root> *v, atrium_buf *out);
//! int  <root>_encode_frame(const struct <root> *v, atrium_buf *out);
//! int  <root>_decode(const uint8_t *p, size_t n, struct <root> *out, atrium_arena *a);
//! int  <root>_decode_frame(const uint8_t *p, size_t n, size_t max, struct <root> *out, atrium_arena *a);
//! ```
//!
//! The decoder makes the same checks as the Rust one, in the same order
//! (bounds before allocation, ranges, UTF-8, control characters, variants,
//! depth, exact length), and returns the same error for the same bytes:
//! `tests/c_parity.rs` holds it to that. Strings and byte strings are
//! copied into the arena (strings NUL-terminated); `atrium_arena_free`
//! frees everything a decode made.
//!
//! Mapping: integers → `<stdint.h>` types, `bool`, `float`/`double`;
//! `String` → `atrium_str`; `Vec<u8>` → `atrium_bytes`; `Vec<T>` →
//! `struct { T *items; size_t len; }`; `Option<Message>` → a pointer (NULL
//! for none); other `Option<T>` → `struct { bool present; T value; }`;
//! `[T; N]` → `T name[N]`; a struct → `struct`; an enum → `struct { uint32_t
//! tag; union { ... } u; }` with `<ENUM>_<VARIANT>` tag constants.

use std::collections::BTreeSet;
use std::fmt::Write;

use serde_json::Value;

#[derive(Debug, Clone)]
enum T {
    Prim { name: String, min: Option<i128>, max: Option<i128> },
    Str { max_len: u64, no_control: bool },
    Vec { item: Box<T>, max_len: u64 },
    Opt(Box<T>),
    Arr { item: Box<T>, len: u64 },
    Ref(String),
}

#[derive(Debug, Clone)]
struct Field {
    name: String,
    ty:   T,
}

#[derive(Debug, Clone)]
enum Def {
    Struct { name: String, fields: Vec<Field> },
    Enum { name: String, variants: Vec<(String, Vec<Field>)> },
}

impl Def {
    fn name(&self) -> &str {
        match self {
            Def::Struct { name, .. } | Def::Enum { name, .. } => name,
        }
    }
}

fn err<X>(m: impl Into<String>) -> Result<X, String> { Err(m.into()) }

fn num(v: &Value) -> Option<i128> {
    v.as_i64().map(|x| x as i128).or_else(|| v.as_u64().map(|x| x as i128))
}

fn parse_ty(v: &Value) -> Result<T, String> {
    let o = v.as_object().ok_or("a type is an object")?;
    if let Some(p) = o.get("prim") {
        let name = p.as_str().ok_or("prim: a name")?.to_string();
        if !PRIMS.contains(&name.as_str()) {
            return err(format!("unknown prim {name}"));
        }
        return Ok(T::Prim { name, min: o.get("min").and_then(num), max: o.get("max").and_then(num) });
    }
    if let Some(s) = o.get("string") {
        return Ok(T::Str {
            max_len: s.get("max_len").and_then(Value::as_u64).ok_or("string: max_len")?,
            no_control: s.get("no_control").and_then(Value::as_bool).unwrap_or(false),
        });
    }
    if let Some(i) = o.get("vec") {
        return Ok(T::Vec { item: Box::new(parse_ty(i)?), max_len: o.get("max_len").and_then(Value::as_u64).ok_or("vec: max_len")? });
    }
    if let Some(i) = o.get("option") {
        return Ok(T::Opt(Box::new(parse_ty(i)?)));
    }
    if let Some(i) = o.get("array") {
        return Ok(T::Arr { item: Box::new(parse_ty(i)?), len: o.get("len").and_then(Value::as_u64).ok_or("array: len")? });
    }
    if let Some(r) = o.get("ref") {
        return Ok(T::Ref(r.as_str().ok_or("ref: a name")?.to_string()));
    }
    err(format!("unknown type {v}"))
}

fn parse_fields(v: &Value) -> Result<Vec<Field>, String> {
    v.as_array().ok_or("fields: a list")?.iter().map(|f| {
        Ok(Field {
            name: f.get("name").and_then(Value::as_str).ok_or("field: name")?.to_string(),
            ty: parse_ty(f.get("ty").ok_or("field: ty")?)?,
        })
    }).collect()
}

const PRIMS: &[&str] = &["u8", "u16", "u32", "u64", "i8", "i16", "i32", "i64", "f32", "f64", "bool"];

const KEYWORDS: &[&str] = &["auto", "break", "case", "char", "const", "continue", "default", "do", "double", "else", "enum",
    "extern", "float", "for", "goto", "if", "inline", "int", "long", "register", "restrict", "return", "short", "signed",
    "sizeof", "static", "struct", "switch", "typedef", "union", "unsigned", "void", "volatile", "while", "bool", "true",
    "false", "_Bool", "u", "tag", "items", "len", "present", "value", "ptr"];

fn snake(s: &str) -> String {
    let mut o = String::new();
    let cs: Vec<char> = s.chars().collect();
    for (i, c) in cs.iter().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 && (cs[i - 1].is_ascii_lowercase() || cs[i - 1].is_ascii_digit()
                || (cs[i - 1].is_ascii_uppercase() && cs.get(i + 1).is_some_and(|n| n.is_ascii_lowercase()))) {
                o.push('_');
            }
            o.push(c.to_ascii_lowercase());
        } else if c.is_ascii_alphanumeric() || *c == '_' {
            o.push(*c);
        }
    }
    o
}

fn ident(s: &str) -> String {
    let s = snake(s);
    if KEYWORDS.contains(&s.as_str()) || s.starts_with(|c: char| c.is_ascii_digit()) { format!("{s}_") } else { s }
}

struct Gen<'a> {
    prefix: String,
    defs:   &'a [Def],
    h:      String,
    c:      String,
    tmp:    usize,
}

impl Gen<'_> {
    fn cname(&self, rust: &str) -> String { format!("{}{}", self.prefix, snake(rust)) }

    fn def(&self, n: &str) -> Result<&Def, String> {
        self.defs.iter().find(|d| d.name() == n).ok_or_else(|| format!("no type {n} in the schema"))
    }

    fn is_bytes(t: &T) -> bool {
        matches!(t, T::Vec { item, .. } if matches!(&**item, T::Prim { name, min: None, max: None } if name == "u8"))
    }

    fn prim_c(n: &str) -> &'static str {
        match n {
            "u8" => "uint8_t", "u16" => "uint16_t", "u32" => "uint32_t", "u64" => "uint64_t",
            "i8" => "int8_t", "i16" => "int16_t", "i32" => "int32_t", "i64" => "int64_t",
            "f32" => "float", "f64" => "double", _ => "bool",
        }
    }

    /// A C declaration of `name` with type `t`.
    fn decl(&self, t: &T, name: &str) -> String {
        match t {
            T::Prim { name: p, .. } => format!("{} {name}", Self::prim_c(p)),
            T::Str { .. } => format!("atrium_str {name}"),
            _ if Self::is_bytes(t) => format!("atrium_bytes {name}"),
            T::Vec { item, .. } => format!("struct {{ {}; size_t len; }} {name}", self.decl(item, "(*items)")),
            T::Opt(i) => match &**i {
                T::Ref(r) => format!("struct {} *{name}", self.cname(r)),
                other => format!("struct {{ bool present; {}; }} {name}", self.decl(other, "value")),
            },
            T::Arr { item, len } => self.decl(item, &format!("{name}[{len}]")),
            T::Ref(r) => format!("struct {} {name}", self.cname(r)),
        }
    }

    /// Message types `t` holds by value (so must be defined before).
    fn by_value(t: &T, out: &mut Vec<String>) {
        match t {
            T::Ref(r) => out.push(r.clone()),
            T::Arr { item, .. } => Self::by_value(item, out),
            T::Opt(i) if !matches!(&**i, T::Ref(_)) => Self::by_value(i, out),
            _ => {}
        }
    }

    /// The fewest bytes a value of `t` can take.
    fn min_size(&self, t: &T, seen: &mut Vec<String>) -> u64 {
        match t {
            T::Prim { name, .. } => match name.as_str() { "f32" => 4, "f64" => 8, _ => 1 },
            T::Str { .. } | T::Vec { .. } | T::Opt(_) => 1,
            T::Arr { item, len } => len.saturating_mul(self.min_size(item, seen)),
            T::Ref(r) => {
                if seen.contains(r) {
                    return 1;
                }
                seen.push(r.clone());
                let s = match self.def(r) {
                    Ok(Def::Struct { fields, .. }) => fields.iter().map(|f| self.min_size(&f.ty, seen)).sum(),
                    _ => 1,
                };
                seen.pop();
                s
            }
        }
    }

    fn t(&mut self) -> String {
        self.tmp += 1;
        format!("t{}", self.tmp)
    }

    fn enc(&mut self, t: &T, e: &str, o: &mut String, ind: &str) {
        match t {
            T::Prim { name, .. } => match name.as_str() {
                "u8" | "i8" => writeln!(o, "{ind}atrium_put_byte(b, (uint8_t)({e}));").unwrap(),
                "bool" => writeln!(o, "{ind}atrium_put_byte(b, ({e}) ? 1 : 0);").unwrap(),
                "u16" | "u32" | "u64" => writeln!(o, "{ind}atrium_put_varint(b, (uint64_t)({e}));").unwrap(),
                "i16" | "i32" | "i64" => writeln!(o, "{ind}atrium_put_varint(b, atrium_zigzag((int64_t)({e})));").unwrap(),
                "f32" => writeln!(o, "{ind}atrium_put_f32(b, {e});").unwrap(),
                _ => writeln!(o, "{ind}atrium_put_f64(b, {e});").unwrap(),
            },
            T::Str { .. } => writeln!(o, "{ind}atrium_put_raw(b, (const uint8_t *)({e}).ptr, ({e}).len);").unwrap(),
            _ if Self::is_bytes(t) => writeln!(o, "{ind}atrium_put_raw(b, ({e}).ptr, ({e}).len);").unwrap(),
            T::Vec { item, .. } => {
                let i = self.t();
                writeln!(o, "{ind}atrium_put_varint(b, (uint64_t)({e}).len);").unwrap();
                writeln!(o, "{ind}for (size_t {i} = 0; {i} < ({e}).len; {i}++) {{").unwrap();
                self.enc(item, &format!("({e}).items[{i}]"), o, &format!("{ind}    "));
                writeln!(o, "{ind}}}").unwrap();
            }
            T::Opt(inner) => match &**inner {
                T::Ref(r) => {
                    let n = self.cname(r);
                    writeln!(o, "{ind}if ({e}) {{ atrium_put_byte(b, 1); enc_{n}({e}, b); }} else atrium_put_byte(b, 0);").unwrap();
                }
                other => {
                    writeln!(o, "{ind}if (({e}).present) {{").unwrap();
                    writeln!(o, "{ind}    atrium_put_byte(b, 1);").unwrap();
                    self.enc(other, &format!("({e}).value"), o, &format!("{ind}    "));
                    writeln!(o, "{ind}}} else atrium_put_byte(b, 0);").unwrap();
                }
            },
            T::Arr { item, len } => {
                let i = self.t();
                writeln!(o, "{ind}for (size_t {i} = 0; {i} < {len}; {i}++) {{").unwrap();
                self.enc(item, &format!("({e})[{i}]"), o, &format!("{ind}    "));
                writeln!(o, "{ind}}}").unwrap();
            }
            T::Ref(r) => {
                let n = self.cname(r);
                writeln!(o, "{ind}enc_{n}(&({e}), b);").unwrap();
            }
        }
    }

    fn range(o: &mut String, ind: &str, v: &str, signed: bool, min: Option<i128>, max: Option<i128>) {
        let lit = |x: i128| if signed { format!("INT64_C({x})") } else { format!("UINT64_C({x})") };
        let mut conds = Vec::new();
        if let Some(m) = min {
            if signed || m > 0 {
                conds.push(format!("{v} < {}", if signed { lit(m.max(i64::MIN as i128)) } else { lit(m.min(u64::MAX as i128)) }));
            }
        }
        if let Some(m) = max {
            if m < 0 && !signed {
                conds.push("1".into());
            } else {
                conds.push(format!("{v} > {}", if signed { lit(m.min(i64::MAX as i128)) } else { lit(m.min(u64::MAX as i128)) }));
            }
        }
        if !conds.is_empty() {
            writeln!(o, "{ind}if ({}) return ATRIUM_MSG_OUT_OF_RANGE;", conds.join(" || ")).unwrap();
        }
    }

    fn dec(&mut self, t: &T, lhs: &str, o: &mut String, ind: &str) -> Result<(), String> {
        let ind2 = format!("{ind}    ");
        match t {
            T::Prim { name, min, max } => {
                let v = self.t();
                writeln!(o, "{ind}{{").unwrap();
                match name.as_str() {
                    "u8" => {
                        writeln!(o, "{ind2}uint8_t {v}; ATRIUM_TRY(atrium_rd_byte(r, &{v}));").unwrap();
                        Self::range(o, &ind2, &format!("(uint64_t){v}"), false, *min, *max);
                    }
                    "i8" => {
                        writeln!(o, "{ind2}uint8_t {v}u; ATRIUM_TRY(atrium_rd_byte(r, &{v}u)); int64_t {v} = (int8_t){v}u;").unwrap();
                        Self::range(o, &ind2, &v, true, *min, *max);
                    }
                    "bool" => {
                        writeln!(o, "{ind2}uint8_t {v}; ATRIUM_TRY(atrium_rd_byte(r, &{v}));").unwrap();
                        writeln!(o, "{ind2}if ({v} > 1) return ATRIUM_MSG_BAD_TAG;").unwrap();
                    }
                    "u16" | "u32" | "u64" => {
                        let bits = &name[1..];
                        writeln!(o, "{ind2}uint64_t {v}; ATRIUM_TRY(atrium_rd_varint(r, {bits}, &{v}));").unwrap();
                        Self::range(o, &ind2, &v, false, *min, *max);
                    }
                    "i16" | "i32" | "i64" => {
                        let bits = &name[1..];
                        writeln!(o, "{ind2}uint64_t {v}z; ATRIUM_TRY(atrium_rd_varint(r, {bits}, &{v}z));").unwrap();
                        writeln!(o, "{ind2}int64_t {v} = atrium_unzigzag({v}z);").unwrap();
                        Self::range(o, &ind2, &v, true, *min, *max);
                    }
                    "f32" => writeln!(o, "{ind2}float {v}; ATRIUM_TRY(atrium_rd_f32(r, &{v}));").unwrap(),
                    _ => writeln!(o, "{ind2}double {v}; ATRIUM_TRY(atrium_rd_f64(r, &{v}));").unwrap(),
                }
                writeln!(o, "{ind2}{lhs} = ({}){v};", Self::prim_c(name)).unwrap();
                writeln!(o, "{ind}}}").unwrap();
            }
            T::Str { max_len, no_control } => {
                writeln!(o, "{ind}ATRIUM_TRY(atrium_rd_str(r, UINT64_C({max_len}), {}, &({lhs})));", if *no_control { 1 } else { 0 }).unwrap();
            }
            _ if Self::is_bytes(t) => {
                let T::Vec { max_len, .. } = t else { unreachable!() };
                writeln!(o, "{ind}ATRIUM_TRY(atrium_rd_bytes(r, UINT64_C({max_len}), &({lhs})));").unwrap();
            }
            T::Vec { item, max_len } => {
                let (n, cap, i) = (self.t(), self.t(), self.t());
                let min = self.min_size(item, &mut Vec::new());
                writeln!(o, "{ind}{{").unwrap();
                writeln!(o, "{ind2}uint64_t {n}; ATRIUM_TRY(atrium_rd_varint(r, 64, &{n}));").unwrap();
                writeln!(o, "{ind2}if ({n} > UINT64_C({max_len})) return ATRIUM_MSG_TOO_LONG;").unwrap();
                // As the Rust decoder: never reserve more than the input could hold.
                if min > 0 {
                    writeln!(o, "{ind2}uint64_t {cap} = {n} < (uint64_t)(r->n - r->pos) ? {n} : (uint64_t)(r->n - r->pos);").unwrap();
                } else {
                    writeln!(o, "{ind2}uint64_t {cap} = {n};").unwrap();
                }
                writeln!(o, "{ind2}({lhs}).len = 0;").unwrap();
                writeln!(o, "{ind2}({lhs}).items = atrium_arena_array(r->a, {cap}, sizeof(*({lhs}).items));").unwrap();
                writeln!(o, "{ind2}if ({cap} && !({lhs}).items) return ATRIUM_MSG_NO_MEMORY;").unwrap();
                writeln!(o, "{ind2}for (uint64_t {i} = 0; {i} < {n}; {i}++) {{").unwrap();
                writeln!(o, "{ind2}    if ({i} >= {cap}) return ATRIUM_MSG_TRUNCATED;").unwrap();
                self.dec(item, &format!("({lhs}).items[{i}]"), o, &format!("{ind2}    "))?;
                writeln!(o, "{ind2}    ({lhs}).len = (size_t){i} + 1;").unwrap();
                writeln!(o, "{ind2}}}").unwrap();
                writeln!(o, "{ind}}}").unwrap();
            }
            T::Opt(inner) => {
                let tag = self.t();
                writeln!(o, "{ind}{{").unwrap();
                writeln!(o, "{ind2}uint8_t {tag}; ATRIUM_TRY(atrium_rd_byte(r, &{tag}));").unwrap();
                writeln!(o, "{ind2}if ({tag} > 1) return ATRIUM_MSG_BAD_TAG;").unwrap();
                match &**inner {
                    T::Ref(rn) => {
                        let n = self.cname(rn);
                        writeln!(o, "{ind2}{lhs} = NULL;").unwrap();
                        writeln!(o, "{ind2}if ({tag}) {{").unwrap();
                        writeln!(o, "{ind2}    struct {n} *p = (struct {n} *)atrium_arena_array(r->a, 1, sizeof(struct {n}));").unwrap();
                        writeln!(o, "{ind2}    if (!p) return ATRIUM_MSG_NO_MEMORY;").unwrap();
                        writeln!(o, "{ind2}    ATRIUM_TRY(dec_{n}(r, p));").unwrap();
                        writeln!(o, "{ind2}    {lhs} = p;").unwrap();
                        writeln!(o, "{ind2}}}").unwrap();
                    }
                    other => {
                        writeln!(o, "{ind2}({lhs}).present = {tag} == 1;").unwrap();
                        writeln!(o, "{ind2}if ({tag}) {{").unwrap();
                        self.dec(other, &format!("({lhs}).value"), o, &format!("{ind2}    "))?;
                        writeln!(o, "{ind2}}}").unwrap();
                    }
                }
                writeln!(o, "{ind}}}").unwrap();
            }
            T::Arr { item, len } => {
                let i = self.t();
                writeln!(o, "{ind}for (size_t {i} = 0; {i} < {len}; {i}++) {{").unwrap();
                self.dec(item, &format!("({lhs})[{i}]"), o, &ind2)?;
                writeln!(o, "{ind}}}").unwrap();
            }
            T::Ref(rn) => {
                self.def(rn)?;
                let n = self.cname(rn);
                writeln!(o, "{ind}ATRIUM_TRY(dec_{n}(r, &({lhs})));").unwrap();
            }
        }
        Ok(())
    }
}

/// Generate `(header, source)` from a schema file's text. `prefix` goes
/// before every C name (e.g. "photos_").
pub fn generate(schema: &str, prefix: &str, header_name: &str) -> Result<(String, String), String> {
    let v: Value = serde_json::from_str(schema).map_err(|e| format!("schema: {e}"))?;
    if v.get("schema").and_then(Value::as_str) != Some("atrium-message/1") {
        return err("not an atrium-message/1 schema");
    }
    let root = v.get("root").and_then(Value::as_str).ok_or("root")?.to_string();
    let version = v.get("version").and_then(Value::as_u64).ok_or("version")?;
    let mut defs = Vec::new();
    for t in v.get("types").and_then(Value::as_array).ok_or("types")? {
        let name = t.get("name").and_then(Value::as_str).ok_or("type: name")?.to_string();
        defs.push(match t.get("kind").and_then(Value::as_str) {
            Some("struct") => Def::Struct { name, fields: parse_fields(t.get("fields").ok_or("struct: fields")?)? },
            Some("enum") => Def::Enum {
                name,
                variants: t.get("variants").and_then(Value::as_array).ok_or("enum: variants")?.iter().map(|vv| {
                    Ok((vv.get("name").and_then(Value::as_str).ok_or("variant: name")?.to_string(),
                        parse_fields(vv.get("fields").ok_or("variant: fields")?)?))
                }).collect::<Result<_, String>>()?,
            },
            _ => return err("type kind: struct or enum"),
        });
    }
    let mut g = Gen { prefix: prefix.to_string(), defs: &defs, h: String::new(), c: String::new(), tmp: 0 };
    g.def(&root)?;

    // Order: what a type holds by value first.
    let mut order: Vec<String> = Vec::new();
    let mut visiting = BTreeSet::new();
    fn visit(g: &Gen, n: &str, order: &mut Vec<String>, visiting: &mut BTreeSet<String>) -> Result<(), String> {
        if order.iter().any(|o| o == n) {
            return Ok(());
        }
        if !visiting.insert(n.to_string()) {
            return err(format!("{n} holds itself by value"));
        }
        let mut deps = Vec::new();
        match g.def(n)? {
            Def::Struct { fields, .. } => fields.iter().for_each(|f| Gen::by_value(&f.ty, &mut deps)),
            Def::Enum { variants, .. } => variants.iter().flat_map(|v| &v.1).for_each(|f| Gen::by_value(&f.ty, &mut deps)),
        }
        for d in deps {
            visit(g, &d, order, visiting)?;
        }
        visiting.remove(n);
        order.push(n.to_string());
        Ok(())
    }
    for d in &defs {
        visit(&g, d.name(), &mut order, &mut visiting)?;
    }

    let guard = format!("{}_H", header_name.to_ascii_uppercase().replace(|c: char| !c.is_ascii_alphanumeric(), "_"));
    let rn = g.cname(&root);
    let mut h = String::new();
    writeln!(h, "/* Generated by atrium-schema from the {root} schema (version {version}). Do not edit. */").unwrap();
    writeln!(h, "#ifndef {guard}\n#define {guard}\n").unwrap();
    h.push_str(RUNTIME_H);
    writeln!(h).unwrap();
    for d in &defs {
        writeln!(h, "struct {};", g.cname(d.name())).unwrap();
    }
    writeln!(h).unwrap();
    for n in &order {
        let d = g.def(n)?.clone();
        let cn = g.cname(n);
        match &d {
            Def::Struct { fields, .. } => {
                writeln!(h, "struct {cn} {{").unwrap();
                if fields.is_empty() {
                    writeln!(h, "    char unused_;").unwrap();
                }
                for f in fields {
                    writeln!(h, "    {};", g.decl(&f.ty, &ident(&f.name))).unwrap();
                }
                writeln!(h, "}};\n").unwrap();
            }
            Def::Enum { variants, .. } => {
                for (i, (vn, _)) in variants.iter().enumerate() {
                    writeln!(h, "#define {}_{} {i}u", cn.to_ascii_uppercase(), snake(vn).to_ascii_uppercase()).unwrap();
                }
                writeln!(h, "struct {cn} {{\n    uint32_t tag;\n    union {{").unwrap();
                for (vn, fs) in variants {
                    if fs.is_empty() {
                        continue;
                    }
                    writeln!(h, "        struct {{").unwrap();
                    for f in fs {
                        writeln!(h, "            {};", g.decl(&f.ty, &ident(&f.name))).unwrap();
                    }
                    writeln!(h, "        }} {};", ident(vn)).unwrap();
                }
                writeln!(h, "        char unused_;\n    }} u;\n}};\n").unwrap();
            }
        }
    }
    writeln!(h, "#define {}_VERSION {version}u\n", rn.to_ascii_uppercase()).unwrap();
    writeln!(h, "int {rn}_encode(const struct {rn} *v, atrium_buf *out);").unwrap();
    writeln!(h, "int {rn}_encode_frame(const struct {rn} *v, atrium_buf *out);").unwrap();
    writeln!(h, "int {rn}_decode(const uint8_t *p, size_t n, struct {rn} *out, atrium_arena *a);").unwrap();
    writeln!(h, "int {rn}_decode_frame(const uint8_t *p, size_t n, size_t max, struct {rn} *out, atrium_arena *a);").unwrap();
    writeln!(h, "\n#endif").unwrap();

    let mut c = String::new();
    writeln!(c, "/* Generated by atrium-schema from the {root} schema (version {version}). Do not edit. */").unwrap();
    writeln!(c, "#include \"{header_name}\"\n").unwrap();
    for d in &defs {
        let cn = g.cname(d.name());
        writeln!(c, "static void enc_{cn}(const struct {cn} *v, atrium_buf *b);").unwrap();
        writeln!(c, "static int dec_{cn}(atrium_rd *r, struct {cn} *v);").unwrap();
    }
    writeln!(c).unwrap();
    for d in defs.iter() {
        let cn = g.cname(d.name());
        let mut e = String::new();
        let mut dd = String::new();
        match d {
            Def::Struct { fields, .. } => {
                for f in fields {
                    g.enc(&f.ty, &format!("v->{}", ident(&f.name)), &mut e, "    ");
                    g.dec(&f.ty, &format!("v->{}", ident(&f.name)), &mut dd, "    ")?;
                }
                writeln!(c, "static void enc_{cn}(const struct {cn} *v, atrium_buf *b) {{\n    (void)v; (void)b;\n{e}}}\n").unwrap();
                writeln!(c, "static int dec_{cn}(atrium_rd *r, struct {cn} *v) {{\n    ATRIUM_TRY(atrium_rd_enter(r));\n{dd}    r->depth--;\n    return 0;\n}}\n").unwrap();
            }
            Def::Enum { variants, .. } => {
                let up = cn.to_ascii_uppercase();
                writeln!(e, "    atrium_put_varint(b, v->tag);\n    switch (v->tag) {{").unwrap();
                writeln!(dd, "    uint64_t tag; ATRIUM_TRY(atrium_rd_varint(r, 32, &tag));\n    switch (tag) {{").unwrap();
                for (vn, fs) in variants {
                    let k = format!("{up}_{}", snake(vn).to_ascii_uppercase());
                    writeln!(e, "    case {k}:").unwrap();
                    writeln!(dd, "    case {k}:\n        v->tag = {k};").unwrap();
                    for f in fs {
                        let place = format!("v->u.{}.{}", ident(vn), ident(&f.name));
                        g.enc(&f.ty, &place, &mut e, "        ");
                        g.dec(&f.ty, &place, &mut dd, "        ")?;
                    }
                    writeln!(e, "        break;").unwrap();
                    writeln!(dd, "        break;").unwrap();
                }
                writeln!(e, "    default: b->failed = 1;\n    }}").unwrap();
                writeln!(dd, "    default: return ATRIUM_MSG_BAD_VARIANT;\n    }}").unwrap();
                writeln!(c, "static void enc_{cn}(const struct {cn} *v, atrium_buf *b) {{\n{e}}}\n").unwrap();
                writeln!(c, "static int dec_{cn}(atrium_rd *r, struct {cn} *v) {{\n    ATRIUM_TRY(atrium_rd_enter(r));\n    memset(v, 0, sizeof(*v));\n{dd}    r->depth--;\n    return 0;\n}}\n").unwrap();
            }
        }
    }
    let up = rn.to_ascii_uppercase();
    write!(c, "int {rn}_encode(const struct {rn} *v, atrium_buf *out) {{
    enc_{rn}(v, out);
    return out->failed ? ATRIUM_MSG_NO_MEMORY : 0;
}}

int {rn}_encode_frame(const struct {rn} *v, atrium_buf *out) {{
    atrium_put_varint(out, {up}_VERSION);
    return {rn}_encode(v, out);
}}

int {rn}_decode(const uint8_t *p, size_t n, struct {rn} *out, atrium_arena *a) {{
    atrium_rd r = {{ p, n, 0, 0, a }};
    memset(out, 0, sizeof(*out));
    ATRIUM_TRY(dec_{rn}(&r, out));
    return r.pos == r.n ? 0 : ATRIUM_MSG_TRAILING;
}}

int {rn}_decode_frame(const uint8_t *p, size_t n, size_t max, struct {rn} *out, atrium_arena *a) {{
    if (n > max) return ATRIUM_MSG_FRAME_TOO_LARGE;
    atrium_rd r = {{ p, n, 0, 0, a }};
    uint64_t ver; ATRIUM_TRY(atrium_rd_varint(&r, 32, &ver));
    if (ver != {up}_VERSION) return ATRIUM_MSG_VERSION;
    memset(out, 0, sizeof(*out));
    ATRIUM_TRY(dec_{rn}(&r, out));
    return r.pos == r.n ? 0 : ATRIUM_MSG_TRAILING;
}}
").unwrap();
    g.h = h;
    g.c = c;
    Ok((g.h, g.c))
}

/// The runtime every generated header carries (once, by its guard).
const RUNTIME_H: &str = r#"#ifndef ATRIUM_MESSAGE_RUNTIME
#define ATRIUM_MESSAGE_RUNTIME
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

/* Errors: the same refusals as atrium_message::Error. */
enum {
    ATRIUM_MSG_OK = 0,
    ATRIUM_MSG_TRUNCATED = -1,
    ATRIUM_MSG_TRAILING = -2,
    ATRIUM_MSG_BAD_VARINT = -3,
    ATRIUM_MSG_BAD_TAG = -4,
    ATRIUM_MSG_TOO_LONG = -5,
    ATRIUM_MSG_OUT_OF_RANGE = -6,
    ATRIUM_MSG_BAD_UTF8 = -7,
    ATRIUM_MSG_CONTROL_CHAR = -8,
    ATRIUM_MSG_BAD_VARIANT = -9,
    ATRIUM_MSG_TOO_DEEP = -10,
    ATRIUM_MSG_VERSION = -11,
    ATRIUM_MSG_FRAME_TOO_LARGE = -12,
    ATRIUM_MSG_NO_MEMORY = -13
};
#define ATRIUM_MSG_MAX_DEPTH 64u

typedef struct { const char *ptr; size_t len; } atrium_str;      /* NUL-terminated too */
typedef struct { const uint8_t *ptr; size_t len; } atrium_bytes;

/* Everything a decode allocates; atrium_arena_free releases it all. */
typedef struct atrium_arena_block { struct atrium_arena_block *next; } atrium_arena_block;
typedef struct { atrium_arena_block *head; } atrium_arena;

static inline void *atrium_arena_array(atrium_arena *a, uint64_t n, size_t size) {
    if (n == 0) return NULL;
    if (size && n > (SIZE_MAX - 64) / size) return NULL;
    atrium_arena_block *b = (atrium_arena_block *)malloc(sizeof(atrium_arena_block) + 16 + (size_t)n * size);
    if (!b) return NULL;
    b->next = a->head;
    a->head = b;
    void *p = (char *)b + ((sizeof(atrium_arena_block) + 15) & ~(size_t)15);
    memset(p, 0, (size_t)n * size);
    return p;
}
static inline void atrium_arena_free(atrium_arena *a) {
    while (a->head) { atrium_arena_block *n = a->head->next; free(a->head); a->head = n; }
}

/* A growing output buffer; `failed` once an allocation failed. */
typedef struct { uint8_t *ptr; size_t len, cap; int failed; } atrium_buf;
static inline void atrium_buf_free(atrium_buf *b) { free(b->ptr); b->ptr = NULL; b->len = b->cap = 0; }
static inline void atrium_put_raw_bytes(atrium_buf *b, const uint8_t *p, size_t n) {
    if (b->failed) return;
    if (n > SIZE_MAX - b->len) { b->failed = 1; return; }
    if (b->len + n > b->cap) {
        size_t c = b->cap ? b->cap : 64;
        while (c < b->len + n) { if (c > SIZE_MAX / 2) { c = b->len + n; break; } c *= 2; }
        uint8_t *q = (uint8_t *)realloc(b->ptr, c);
        if (!q) { b->failed = 1; return; }
        b->ptr = q; b->cap = c;
    }
    if (n) memcpy(b->ptr + b->len, p, n);
    b->len += n;
}
static inline void atrium_put_byte(atrium_buf *b, uint8_t x) { atrium_put_raw_bytes(b, &x, 1); }
static inline void atrium_put_varint(atrium_buf *b, uint64_t v) {
    uint8_t t[10]; size_t n = 0;
    do { uint8_t c = (uint8_t)(v & 0x7f); v >>= 7; t[n++] = v ? (uint8_t)(c | 0x80) : c; } while (v);
    atrium_put_raw_bytes(b, t, n);
}
static inline void atrium_put_raw(atrium_buf *b, const uint8_t *p, size_t n) { atrium_put_varint(b, n); atrium_put_raw_bytes(b, p, n); }
static inline uint64_t atrium_zigzag(int64_t v) { return ((uint64_t)v << 1) ^ (uint64_t)(v >> 63); }
static inline int64_t atrium_unzigzag(uint64_t z) { return (int64_t)(z >> 1) ^ -(int64_t)(z & 1); }
static inline void atrium_put_f32(atrium_buf *b, float f) {
    uint32_t u; memcpy(&u, &f, 4); uint8_t t[4];
    for (int i = 0; i < 4; i++) t[i] = (uint8_t)(u >> (8 * i));
    atrium_put_raw_bytes(b, t, 4);
}
static inline void atrium_put_f64(atrium_buf *b, double f) {
    uint64_t u; memcpy(&u, &f, 8); uint8_t t[8];
    for (int i = 0; i < 8; i++) t[i] = (uint8_t)(u >> (8 * i));
    atrium_put_raw_bytes(b, t, 8);
}

typedef struct { const uint8_t *p; size_t n, pos; unsigned depth; atrium_arena *a; } atrium_rd;
#define ATRIUM_TRY(x) do { int atrium_rc_ = (x); if (atrium_rc_) return atrium_rc_; } while (0)

static inline int atrium_rd_enter(atrium_rd *r) { return ++r->depth > ATRIUM_MSG_MAX_DEPTH ? ATRIUM_MSG_TOO_DEEP : 0; }
static inline int atrium_rd_byte(atrium_rd *r, uint8_t *out) {
    if (r->pos >= r->n) return ATRIUM_MSG_TRUNCATED;
    *out = r->p[r->pos++];
    return 0;
}
/* LEB128 of at most `bits` bits, no bits beyond them (as postcard). */
static inline int atrium_rd_varint(atrium_rd *r, unsigned bits, uint64_t *out) {
    unsigned max = (bits + 6) / 7; uint64_t v = 0;
    for (unsigned i = 0; i < max; i++) {
        uint8_t b; ATRIUM_TRY(atrium_rd_byte(r, &b));
        uint64_t chunk = b & 0x7f; unsigned shift = 7 * i;
        if (i == max - 1) {
            unsigned left = bits - shift;
            if ((b & 0x80) || (left < 7 && (chunk >> left))) return ATRIUM_MSG_BAD_VARINT;
        }
        v |= chunk << shift;
        if (!(b & 0x80)) { *out = v; return 0; }
    }
    return ATRIUM_MSG_BAD_VARINT;
}
static inline int atrium_rd_f32(atrium_rd *r, float *out) {
    if (r->n - r->pos < 4) return ATRIUM_MSG_TRUNCATED;
    uint32_t u = 0; for (int i = 0; i < 4; i++) u |= (uint32_t)r->p[r->pos + i] << (8 * i);
    r->pos += 4; memcpy(out, &u, 4); return 0;
}
static inline int atrium_rd_f64(atrium_rd *r, double *out) {
    if (r->n - r->pos < 8) return ATRIUM_MSG_TRUNCATED;
    uint64_t u = 0; for (int i = 0; i < 8; i++) u |= (uint64_t)r->p[r->pos + i] << (8 * i);
    r->pos += 8; memcpy(out, &u, 8); return 0;
}
/* UTF-8 as Rust's from_utf8 takes it; then, if asked, no Cc but tab. */
static inline int atrium_utf8(const uint8_t *s, size_t n, int no_control) {
    size_t i = 0; int bad = 0, ctl = 0;
    while (i < n) {
        uint32_t c = s[i]; size_t k;
        if (c < 0x80) { k = 1; }
        else if (c >= 0xc2 && c <= 0xdf) { k = 2; c &= 0x1f; }
        else if (c >= 0xe0 && c <= 0xef) { k = 3; c &= 0x0f; }
        else if (c >= 0xf0 && c <= 0xf4) { k = 4; c &= 0x07; }
        else return ATRIUM_MSG_BAD_UTF8;
        if (n - i < k) return ATRIUM_MSG_BAD_UTF8;
        for (size_t j = 1; j < k; j++) {
            if ((s[i + j] & 0xc0) != 0x80) return ATRIUM_MSG_BAD_UTF8;
            c = (c << 6) | (s[i + j] & 0x3f);
        }
        if ((k == 3 && (c < 0x800 || (c >= 0xd800 && c <= 0xdfff))) || (k == 4 && (c < 0x10000 || c > 0x10ffff))) bad = 1;
        if (bad) return ATRIUM_MSG_BAD_UTF8;
        if (no_control && ((c < 0x20 && c != 0x09) || (c >= 0x7f && c <= 0x9f))) ctl = 1;
        i += k;
    }
    return ctl ? ATRIUM_MSG_CONTROL_CHAR : 0;
}
static inline int atrium_rd_len(atrium_rd *r, uint64_t max, size_t *out) {
    uint64_t n; ATRIUM_TRY(atrium_rd_varint(r, 64, &n));
    if (n > max) return ATRIUM_MSG_TOO_LONG;
    if (n > r->n - r->pos) return ATRIUM_MSG_TRUNCATED;
    *out = (size_t)n; return 0;
}
static inline int atrium_rd_str(atrium_rd *r, uint64_t max, int no_control, atrium_str *out) {
    size_t n; ATRIUM_TRY(atrium_rd_len(r, max, &n));
    ATRIUM_TRY(atrium_utf8(r->p + r->pos, n, no_control));
    char *s = (char *)atrium_arena_array(r->a, (uint64_t)n + 1, 1);
    if (!s) return ATRIUM_MSG_NO_MEMORY;
    memcpy(s, r->p + r->pos, n); s[n] = 0;
    r->pos += n; out->ptr = s; out->len = n; return 0;
}
static inline int atrium_rd_bytes(atrium_rd *r, uint64_t max, atrium_bytes *out) {
    size_t n; ATRIUM_TRY(atrium_rd_len(r, max, &n));
    uint8_t *s = NULL;
    if (n) { s = (uint8_t *)atrium_arena_array(r->a, n, 1); if (!s) return ATRIUM_MSG_NO_MEMORY; memcpy(s, r->p + r->pos, n); }
    r->pos += n; out->ptr = s; out->len = n; return 0;
}
#endif
"#;
