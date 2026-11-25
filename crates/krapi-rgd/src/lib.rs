//! `#[derive(Rgd)]` — one Rust definition yields the typed struct, its
//! `RgdKind` identity, and `rgd_manifest()` building a `krapi` RGD.

use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Data, DeriveInput, Expr, ExprLit, ExprPath, Fields, Lit, Meta, punctuated::Punctuated,
    token::Comma,
};

#[proc_macro_derive(Rgd, attributes(rgd))]
pub fn derive_rgd(input: TokenStream) -> TokenStream {
    let input = match syn::parse::<DeriveInput>(input) {
        Ok(v) => v,
        Err(e) => return e.to_compile_error().into(),
    };
    match expand(input) {
        Ok(t) => t.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

struct Args {
    kind: String,
    plural: String,
    group: String,
    name: String,
    api_version: String,
    resources: ExprPath,
    status: ExprPath,
}

fn expand(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let args = parse_args(&input)?;
    let name = &input.ident;
    let fields = match &input.data {
        Data::Struct(d) => match &d.fields {
            Fields::Named(f) => &f.named,
            _ => return Err(syn::Error::new_spanned(name, "Rgd requires named fields")),
        },
        _ => return Err(syn::Error::new_spanned(name, "Rgd applies to structs only")),
    };
    let mut inserts = Vec::new();
    for f in fields {
        let ident = f
            .ident
            .as_ref()
            .ok_or_else(|| syn::Error::new_spanned(f, "named field"))?;
        let key = serde_name(f, &ident.to_string());
        if serde_skip(f) {
            continue;
        }
        let ty = &f.ty;
        let schema = schema_for_type(ty);
        inserts.push(quote! { spec.insert(#key.to_owned(), ::serde_json::json!(#schema)); });
    }
    let Args {
        kind,
        plural,
        group,
        name: rgd_name,
        api_version,
        resources,
        status,
    } = args;
    Ok(quote! {
        impl ::krapi::api::RgdKind for #name {
            fn kind() -> &'static str { #kind }
            fn plural_name() -> &'static str { #plural }
            fn group() -> &'static str { #group }
            fn name() -> &'static str { #rgd_name }
        }
        impl #name {
            pub fn rgd_manifest() -> ::krapi::api::ResourceGraphDefinition {
            let mut spec = ::std::collections::BTreeMap::new();
            #(#inserts)*
            ::krapi::api::ResourceGraphDefinition {
                name: #rgd_name.to_owned(),
                uid: String::new(),
                generation: 1,
                spec: ::krapi::api::RgdSpec {
                    schema: ::krapi::api::Schema {
                        kind: #kind.to_owned(),
                        plural: Some(#plural.to_owned()),
                        api_version: #api_version.to_owned(),
                        group: Some(#group.to_owned()),
                        scope: ::krapi::api::Scope::Namespaced,
                        spec,
                        types: ::std::collections::BTreeMap::new(),
                        status: #status(),
                        name_validation: None,
                    },
                    resources: #resources(),
                },
            }
            }
        }
    })
}

fn parse_args(input: &DeriveInput) -> syn::Result<Args> {
    let mut kind = None;
    let mut plural = None;
    let mut group = None;
    let mut name = None;
    let mut api_version = None;
    let mut resources = None;
    let mut status = None;
    for attr in &input.attrs {
        if !attr.path().is_ident("rgd") {
            continue;
        }
        let list = attr.parse_args_with(Punctuated::<Meta, Comma>::parse_terminated)?;
        for meta in list {
            let (key, val) = match meta {
                Meta::NameValue(nv) => (
                    nv.path
                        .get_ident()
                        .map(|i| i.to_string())
                        .unwrap_or_default(),
                    nv.value,
                ),
                other => return Err(syn::Error::new_spanned(other, "expected key = value")),
            };
            let lit = match &val {
                Expr::Lit(ExprLit {
                    lit: Lit::Str(s), ..
                }) => s.value(),
                Expr::Path(p) => {
                    if key == "resources" {
                        resources = Some(p.clone());
                        continue;
                    }
                    if key == "status" {
                        status = Some(p.clone());
                        continue;
                    }
                    return Err(syn::Error::new_spanned(val, "expected string literal"));
                }
                _ => {
                    return Err(syn::Error::new_spanned(
                        val,
                        "expected string literal or path",
                    ));
                }
            };
            match key.as_str() {
                "kind" => kind = Some(lit),
                "plural" => plural = Some(lit),
                "group" => group = Some(lit),
                "name" => name = Some(lit),
                "api_version" => api_version = Some(lit),
                other => {
                    return Err(syn::Error::new_spanned(
                        val,
                        format!("unknown rgd key {other:?}"),
                    ));
                }
            }
        }
    }
    Ok(Args {
        kind: kind.ok_or_else(|| syn::Error::new_spanned(&input.ident, "missing rgd(kind)"))?,
        plural: plural
            .ok_or_else(|| syn::Error::new_spanned(&input.ident, "missing rgd(plural)"))?,
        group: group.ok_or_else(|| syn::Error::new_spanned(&input.ident, "missing rgd(group)"))?,
        name: name.ok_or_else(|| syn::Error::new_spanned(&input.ident, "missing rgd(name)"))?,
        api_version: api_version.unwrap_or_else(|| "v1alpha1".to_owned()),
        resources: resources
            .ok_or_else(|| syn::Error::new_spanned(&input.ident, "missing rgd(resources)"))?,
        status: status
            .ok_or_else(|| syn::Error::new_spanned(&input.ident, "missing rgd(status)"))?,
    })
}

fn serde_name(f: &syn::Field, fallback: &str) -> String {
    for attr in &f.attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        if let Ok(list) = attr.parse_args_with(Punctuated::<Meta, Comma>::parse_terminated) {
            for meta in list {
                if let Meta::NameValue(nv) = meta
                    && nv.path.is_ident("rename")
                    && let Expr::Lit(ExprLit {
                        lit: Lit::Str(s), ..
                    }) = &nv.value
                {
                    return s.value();
                }
            }
        }
    }
    fallback.to_owned()
}

fn serde_skip(f: &syn::Field) -> bool {
    f.attrs.iter().any(|a| {
        if !a.path().is_ident("serde") {
            return false;
        }
        a.parse_args_with(Punctuated::<Meta, Comma>::parse_terminated)
            .map(|list| list.into_iter().any(|m| m.path().is_ident("skip")))
            .unwrap_or(false)
    })
}

fn schema_for_type(ty: &syn::Type) -> String {
    let s = quote!(#ty).to_string().replace(' ', "");
    if let Some(inner) = s.strip_prefix("Option<") {
        return schema_for_str(inner.trim_end_matches('>'));
    }
    if let Some(inner) = s.strip_prefix("Vec<") {
        return format!("[]{}", schema_for_str(inner.trim_end_matches('>')));
    }
    if s.starts_with("BTreeMap<String,String>") || s.starts_with("HashMap<String,String>") {
        return "map[string]string".to_owned();
    }
    schema_for_str(&s)
}

fn schema_for_str(s: &str) -> String {
    match s {
        "String" | "&str" | "str" => "string".to_owned(),
        "bool" => "boolean".to_owned(),
        "i64" | "u64" | "i32" | "u32" | "usize" | "isize" => "integer".to_owned(),
        "f64" | "f32" => "number".to_owned(),
        _ => "string".to_owned(),
    }
}
