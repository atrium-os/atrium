//! `#[derive(Message)]` (see the atrium-message crate).
//!
//! Container: `#[message(version = N)]` (default 1).
//! Fields: `#[message(max_len = E, item_max_len = E, min = E, max = E, no_control)]`.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as Ts;
use quote::{format_ident, quote};
use syn::{parse_macro_input, Data, DeriveInput, Expr, Fields};

#[proc_macro_derive(Message, attributes(message))]
pub fn derive_message(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(&input) {
        Ok(t) => t.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

#[derive(Default)]
struct FieldAttrs {
    max_len:      Option<Expr>,
    item_max_len: Option<Expr>,
    min:          Option<Expr>,
    max:          Option<Expr>,
    no_control:   bool,
}

fn field_attrs(attrs: &[syn::Attribute]) -> syn::Result<FieldAttrs> {
    let mut f = FieldAttrs::default();
    for a in attrs.iter().filter(|a| a.path().is_ident("message")) {
        a.parse_nested_meta(|m| {
            if m.path.is_ident("no_control") {
                f.no_control = true;
                return Ok(());
            }
            let slot = if m.path.is_ident("max_len") {
                &mut f.max_len
            } else if m.path.is_ident("item_max_len") {
                &mut f.item_max_len
            } else if m.path.is_ident("min") {
                &mut f.min
            } else if m.path.is_ident("max") {
                &mut f.max
            } else {
                return Err(m.error("expected max_len, item_max_len, min, max or no_control"));
            };
            *slot = Some(m.value()?.parse()?);
            Ok(())
        })?;
    }
    Ok(f)
}

fn version(attrs: &[syn::Attribute]) -> syn::Result<Ts> {
    let mut v: Option<Expr> = None;
    for a in attrs.iter().filter(|a| a.path().is_ident("message")) {
        a.parse_nested_meta(|m| {
            if m.path.is_ident("version") {
                v = Some(m.value()?.parse()?);
                Ok(())
            } else {
                Err(m.error("expected version = N"))
            }
        })?;
    }
    Ok(match v {
        Some(e) => quote!(#e),
        None => quote!(1),
    })
}

fn bounds(path: &str, a: &FieldAttrs) -> Ts {
    let opt_u64 = |e: &Option<Expr>| match e {
        Some(e) => quote!(::core::option::Option::Some((#e) as u64)),
        None => quote!(::core::option::Option::None),
    };
    let opt_i128 = |e: &Option<Expr>| match e {
        Some(e) => quote!(::core::option::Option::Some((#e) as i128)),
        None => quote!(::core::option::Option::None),
    };
    let (ml, iml, mn, mx, nc) = (opt_u64(&a.max_len), opt_u64(&a.item_max_len), opt_i128(&a.min), opt_i128(&a.max), a.no_control);
    quote!(::atrium_message::__private::Bounds {
        field: #path, max_len: #ml, item_max_len: #iml, min: #mn, max: #mx, no_control: #nc,
    })
}

/// (binding name, field name in the schema, type, bounds) for each field.
fn fields(owner: &str, f: &Fields) -> syn::Result<Vec<(syn::Ident, String, syn::Type, Ts)>> {
    let mut v = Vec::new();
    for (i, field) in f.iter().enumerate() {
        let name = field.ident.as_ref().map(|i| i.to_string()).unwrap_or_else(|| format!("_{i}"));
        let bind = format_ident!("f_{}", name.trim_start_matches('_'));
        let a = field_attrs(&field.attrs)?;
        v.push((bind, name.clone(), field.ty.clone(), bounds(&format!("{owner}.{name}"), &a)));
    }
    Ok(v)
}

fn pattern(path: Ts, f: &Fields, binds: &[syn::Ident]) -> Ts {
    match f {
        Fields::Named(n) => {
            let names: Vec<_> = n.named.iter().map(|x| x.ident.clone().unwrap()).collect();
            quote!(#path { #(#names: #binds),* })
        }
        Fields::Unnamed(_) => quote!(#path ( #(#binds),* )),
        Fields::Unit => quote!(#path),
    }
}

fn expand(input: &DeriveInput) -> syn::Result<Ts> {
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(&input.generics, "Message types can't be generic"));
    }
    let ty = &input.ident;
    let name = ty.to_string();
    let ver = version(&input.attrs)?;
    let p = quote!(::atrium_message::__private);
    let (put, get, desc) = match &input.data {
        Data::Struct(s) => {
            let fs = fields(&name, &s.fields)?;
            let binds: Vec<_> = fs.iter().map(|f| f.0.clone()).collect();
            let pat = pattern(quote!(#ty), &s.fields, &binds);
            let puts = fs.iter().map(|(b, _, t, _)| quote!(<#t as #p::Wire>::put(#b, w);));
            let gets = fs.iter().map(|(b, _, t, bo)| quote!(let #b = <#t as #p::Wire>::get(r, &#bo)?;));
            let descs = fs.iter().map(|(_, n, t, bo)| quote!(#p::Field { name: #n, ty: <#t as #p::Wire>::ty(reg, &#bo) }));
            (
                quote!(let #pat = self; #(#puts)*),
                quote!(#(#gets)* r.leave(); Ok(#pat)),
                quote!(#p::TypeDef::Struct { name: #name, fields: vec![#(#descs),*] }),
            )
        }
        Data::Enum(e) => {
            let mut puts = Vec::new();
            let mut gets = Vec::new();
            let mut descs = Vec::new();
            for (i, v) in e.variants.iter().enumerate() {
                if v.discriminant.is_some() {
                    return Err(syn::Error::new_spanned(v, "variants are numbered by position; no explicit discriminants"));
                }
                let vi = &v.ident;
                let vname = vi.to_string();
                let fs = fields(&format!("{name}::{vname}"), &v.fields)?;
                let binds: Vec<_> = fs.iter().map(|f| f.0.clone()).collect();
                let pat = pattern(quote!(#ty::#vi), &v.fields, &binds);
                let idx = i as u64;
                let vput = fs.iter().map(|(b, _, t, _)| quote!(<#t as #p::Wire>::put(#b, w);));
                puts.push(quote!(#pat => { #p::put_varint(w, #idx); #(#vput)* }));
                let vget = fs.iter().map(|(b, _, t, bo)| quote!(let #b = <#t as #p::Wire>::get(r, &#bo)?;));
                gets.push(quote!(#idx => { #(#vget)* #pat }));
                let vdesc = fs.iter().map(|(_, n, t, bo)| quote!(#p::Field { name: #n, ty: <#t as #p::Wire>::ty(reg, &#bo) }));
                descs.push(quote!(#p::Variant { name: #vname, fields: vec![#(#vdesc),*] }));
            }
            (
                quote!(match self { #(#puts)* }),
                quote!(
                    let v = match r.varint(32)? {
                        #(#gets)*
                        other => return Err(#p::Error::BadVariant { ty: #name, index: other }),
                    };
                    r.leave();
                    Ok(v)
                ),
                quote!(#p::TypeDef::Enum { name: #name, variants: vec![#(#descs),*] }),
            )
        }
        Data::Union(_) => return Err(syn::Error::new_spanned(ty, "unions can't be messages")),
    };
    Ok(quote! {
        impl #p::Wire for #ty {
            #[allow(unused_variables)]
            fn put(&self, w: &mut ::std::vec::Vec<u8>) { #put }
            #[allow(unused_variables)]
            fn get(r: &mut #p::Reader<'_>, _b: &#p::Bounds) -> ::core::result::Result<Self, #p::Error> {
                r.enter()?;
                #get
            }
            fn ty(reg: &mut #p::Registry, _b: &#p::Bounds) -> #p::Ty {
                if reg.begin(#name) {
                    let def = #desc;
                    reg.finish(def);
                }
                #p::Ty::Ref(#name)
            }
        }
        impl ::atrium_message::Message for #ty {
            const NAME: &'static str = #name;
            const VERSION: u32 = #ver;
        }
    })
}
