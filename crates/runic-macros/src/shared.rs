use proc_macro2::TokenStream;
use quote::{format_ident, quote};

fn resolve(krate: &str) -> Option<TokenStream> {
    if std::env::var("CARGO_CRATE_NAME").as_deref() == Ok(&krate.replace('-', "_")) {
        return Some(quote!(crate));
    }
    match proc_macro_crate::crate_name(krate) {
        Ok(proc_macro_crate::FoundCrate::Name(name)) => {
            let ident = format_ident!("{name}");
            Some(quote!(::#ident))
        }
        Ok(proc_macro_crate::FoundCrate::Itself) => {
            let ident = format_ident!("{}", krate.replace('-', "_"));
            Some(quote!(::#ident))
        }
        Err(_) => None,
    }
}

pub(crate) fn runic_root() -> TokenStream {
    resolve("runic").unwrap_or_else(|| quote!(::runic))
}

/// Where a leaf crate's items live: the leaf itself when it is a direct
/// dependency, otherwise through the umbrella's re-export of it.
fn leaf(krate: &str, module: TokenStream) -> TokenStream {
    match resolve(krate) {
        Some(path) => path,
        None => {
            let umbrella = runic_root();
            quote!(#umbrella::#module)
        }
    }
}

pub(crate) fn tool_path() -> TokenStream {
    leaf("runic-tool", quote!(tool))
}

pub(crate) fn hook_path() -> TokenStream {
    leaf("runic-hook", quote!(hook))
}

pub(crate) fn state_path() -> TokenStream {
    leaf("runic-state", quote!(state))
}

pub(crate) fn types_path() -> TokenStream {
    leaf("runic-types", quote!(types))
}

pub(crate) fn provider_path() -> TokenStream {
    leaf("runic-provider", quote!(provider))
}

pub(crate) fn dep_path(krate: &str) -> TokenStream {
    match resolve(krate) {
        Some(path) => path,
        None => {
            let umbrella = runic_root();
            let ident = format_ident!("{}", krate.replace('-', "_"));
            quote!(#umbrella::__private::#ident)
        }
    }
}

pub(crate) fn doc_description(attrs: &[syn::Attribute]) -> Option<String> {
    let doc_lines: Vec<String> = attrs
        .iter()
        .filter(|attr| attr.path().is_ident("doc"))
        .filter_map(|attr| {
            if let syn::Meta::NameValue(name_value) = &attr.meta
                && let syn::Expr::Lit(literal) = &name_value.value
                && let syn::Lit::Str(text) = &literal.lit
            {
                return Some(text.value().trim().to_string());
            }
            None
        })
        .collect();
    (!doc_lines.is_empty()).then(|| doc_lines.join(" "))
}

pub(crate) fn ident_value<'a>(
    nv: &'a syn::MetaNameValue,
    expected: &str,
) -> syn::Result<&'a syn::Ident> {
    let syn::Expr::Path(path) = &nv.value else {
        return Err(syn::Error::new_spanned(&nv.value, expected.to_string()));
    };
    path.path
        .get_ident()
        .ok_or_else(|| syn::Error::new_spanned(&nv.value, expected.to_string()))
}

pub(crate) fn string_value(nv: &syn::MetaNameValue, key: &str) -> syn::Result<String> {
    let syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Str(text),
        ..
    }) = &nv.value
    else {
        return Err(syn::Error::new_spanned(
            &nv.value,
            format!("`{key}` must be a string literal"),
        ));
    };
    Ok(text.value())
}

pub(crate) fn snake_case(name: &str) -> String {
    let mut out = String::new();
    for (index, letter) in name.char_indices() {
        if letter.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.extend(letter.to_lowercase());
        } else {
            out.push(letter);
        }
    }
    out
}
