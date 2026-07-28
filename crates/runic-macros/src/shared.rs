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
