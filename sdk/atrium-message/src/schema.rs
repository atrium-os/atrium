//! The schema file: what `atrium-schema` reads to generate C (and other
//! languages later). JSON, written here by hand so the crate has no
//! dependencies:
//!
//! ```json
//! {"schema": "atrium-message/1", "root": "Thumbnail", "version": 1,
//!  "types": [{"name": "Thumbnail", "kind": "struct", "fields": [
//!     {"name": "width", "ty": {"prim": "u32", "min": 1, "max": 4096}},
//!     {"name": "pixels", "ty": {"vec": {"prim": "u8"}, "max_len": 67108864}}]}]}
//! ```
//!
//! A type is `{"prim": p}` (u8 u16 u32 u64 i8 i16 i32 i64 f32 f64 bool,
//! with optional `min`/`max`), `{"string": {"max_len": n, "no_control": b}}`,
//! `{"vec": T, "max_len": n}`, `{"option": T}`, `{"array": T, "len": n}`
//! or `{"ref": "Name"}` for a message type listed in `types`. A struct has
//! `fields`; an enum has `variants`, each with `fields` (empty for a unit
//! variant), encoded as the variant's index then its fields.

use crate::Bounds;

#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Prim { name: &'static str, min: Option<i128>, max: Option<i128> },
    String { max_len: u64, no_control: bool },
    Vec { item: Box<Ty>, max_len: u64 },
    Option(Box<Ty>),
    Array { item: Box<Ty>, len: usize },
    Ref(&'static str),
}

impl Ty {
    pub fn prim(name: &'static str, b: &Bounds) -> Ty { Ty::Prim { name, min: b.min, max: b.max } }
    pub fn string(b: &Bounds) -> Ty { Ty::String { max_len: b.len_or_default(), no_control: b.no_control } }
    pub fn vec(item: Ty, max_len: u64) -> Ty { Ty::Vec { item: Box::new(item), max_len } }
    pub fn option(t: Ty) -> Ty { Ty::Option(Box::new(t)) }
    pub fn array(item: Ty, len: usize) -> Ty { Ty::Array { item: Box::new(item), len } }

    fn json(&self, o: &mut String) {
        match self {
            Ty::Prim { name, min, max } => {
                o.push_str(&format!("{{\"prim\": \"{name}\""));
                if let Some(m) = min {
                    o.push_str(&format!(", \"min\": {m}"));
                }
                if let Some(m) = max {
                    o.push_str(&format!(", \"max\": {m}"));
                }
                o.push('}');
            }
            Ty::String { max_len, no_control } => {
                o.push_str(&format!("{{\"string\": {{\"max_len\": {max_len}, \"no_control\": {no_control}}}}}"))
            }
            Ty::Vec { item, max_len } => {
                o.push_str("{\"vec\": ");
                item.json(o);
                o.push_str(&format!(", \"max_len\": {max_len}}}"));
            }
            Ty::Option(t) => {
                o.push_str("{\"option\": ");
                t.json(o);
                o.push('}');
            }
            Ty::Array { item, len } => {
                o.push_str("{\"array\": ");
                item.json(o);
                o.push_str(&format!(", \"len\": {len}}}"));
            }
            Ty::Ref(n) => o.push_str(&format!("{{\"ref\": \"{n}\"}}")),
        }
    }
}

impl Bounds {
    pub(crate) fn len_or_default(&self) -> u64 { self.max_len.unwrap_or(crate::DEFAULT_MAX_LEN) }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: &'static str,
    pub ty:   Ty,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub name:   &'static str,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeDef {
    Struct { name: &'static str, fields: Vec<Field> },
    Enum { name: &'static str, variants: Vec<Variant> },
}

impl TypeDef {
    pub fn name(&self) -> &'static str {
        match self {
            TypeDef::Struct { name, .. } | TypeDef::Enum { name, .. } => name,
        }
    }
}

/// The message types a schema names, in the order first met.
#[derive(Debug, Default)]
pub struct Registry {
    types:    Vec<TypeDef>,
    visiting: Vec<&'static str>,
}

impl Registry {
    /// Begin describing `name`: false if it is described already (or is
    /// being described, for a type that nests itself through Box/Option/Vec).
    pub fn begin(&mut self, name: &'static str) -> bool {
        if self.types.iter().any(|t| t.name() == name) || self.visiting.contains(&name) {
            return false;
        }
        self.visiting.push(name);
        true
    }
    pub fn finish(&mut self, def: TypeDef) {
        self.visiting.retain(|n| *n != def.name());
        self.types.push(def);
    }
    pub fn types(&self) -> &[TypeDef] { &self.types }

    pub fn to_json(&self, root: &str, version: u32) -> String {
        let mut o = format!("{{\"schema\": \"atrium-message/1\", \"root\": \"{root}\", \"version\": {version}, \"types\": [");
        let fields = |o: &mut String, fs: &[Field]| {
            o.push('[');
            for (i, f) in fs.iter().enumerate() {
                if i > 0 {
                    o.push_str(", ");
                }
                o.push_str(&format!("{{\"name\": \"{}\", \"ty\": ", f.name));
                f.ty.json(o);
                o.push('}');
            }
            o.push(']');
        };
        for (i, t) in self.types.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            o.push_str("\n  ");
            match t {
                TypeDef::Struct { name, fields: fs } => {
                    o.push_str(&format!("{{\"name\": \"{name}\", \"kind\": \"struct\", \"fields\": "));
                    fields(&mut o, fs);
                    o.push('}');
                }
                TypeDef::Enum { name, variants } => {
                    o.push_str(&format!("{{\"name\": \"{name}\", \"kind\": \"enum\", \"variants\": ["));
                    for (j, v) in variants.iter().enumerate() {
                        if j > 0 {
                            o.push_str(", ");
                        }
                        o.push_str(&format!("{{\"name\": \"{}\", \"fields\": ", v.name));
                        fields(&mut o, &v.fields);
                        o.push('}');
                    }
                    o.push_str("]}");
                }
            }
        }
        o.push_str("\n]}\n");
        o
    }
}
