use proc_macro::TokenStream;
use quote::quote;
use syn::{Meta, parse_macro_input};

use crate::shared::runic_root;

struct AbilityAttrs {
    id: Option<String>,
    description: Option<String>,
    deferred: bool,
    activation_span: Option<proc_macro2::Span>,
}

fn valid_ability_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    let edge = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    !bytes.is_empty()
        && bytes.len() <= 64
        && edge(bytes[0])
        && edge(bytes[bytes.len() - 1])
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(*b, b'-' | b'_' | b'.')
        })
}

impl syn::parse::Parse for AbilityAttrs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut attrs = AbilityAttrs {
            id: None,
            description: None,
            deferred: false,
            activation_span: None,
        };
        if input.is_empty() {
            return Ok(attrs);
        }
        let punctuated =
            syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated(input)?;
        for meta in punctuated {
            let Meta::NameValue(nv) = &meta else {
                return Err(syn::Error::new_spanned(
                    &meta,
                    "expected `key = value`; ability takes activation, id, description",
                ));
            };
            let key = nv
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_default();

            if key == "activation" {
                let syn::Expr::Path(p) = &nv.value else {
                    return Err(syn::Error::new_spanned(
                        &nv.value,
                        "expected `eager` or `deferred`",
                    ));
                };
                let ident = p
                    .path
                    .get_ident()
                    .ok_or_else(|| syn::Error::new_spanned(&nv.value, "expected an identifier"))?;
                attrs.deferred = match ident.to_string().as_str() {
                    "eager" => false,
                    "deferred" => true,
                    other => {
                        return Err(syn::Error::new_spanned(
                            &nv.value,
                            format!("unknown activation `{other}`; expected eager or deferred"),
                        ));
                    }
                };
                attrs.activation_span = Some(ident.span());
                continue;
            }

            let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
            else {
                return Err(syn::Error::new_spanned(
                    &nv.value,
                    format!("`{key}` must be a string literal"),
                ));
            };
            match key.as_str() {
                "id" => {
                    if !valid_ability_id(&s.value()) {
                        return Err(syn::Error::new_spanned(
                            &nv.value,
                            "ability id must be 1-64 bytes of lowercase ascii, digits, \
                             `-`, `_` or `.`, starting and ending alphanumeric",
                        ));
                    }
                    attrs.id = Some(s.value());
                }
                "description" => attrs.description = Some(s.value()),
                "name" => {
                    return Err(syn::Error::new_spanned(
                        &nv.path,
                        "`name` is gone — use `id`, which is the same handle everywhere: the \
                         string `load_ability` resolves, the key activation persists under, and \
                         the label errors report",
                    ));
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        &nv.path,
                        format!(
                            "unknown ability attribute `{other}`; expected activation, id, \
                             description"
                        ),
                    ));
                }
            }
        }
        if attrs.deferred {
            let span = attrs.activation_span.unwrap();
            if attrs.id.is_none() {
                return Err(syn::Error::new(
                    span,
                    "activation = deferred needs `id = \"...\"` — it is the handle the model \
                     passes to `load_ability`",
                ));
            }
            if attrs.description.is_none() {
                return Err(syn::Error::new(
                    span,
                    "activation = deferred needs `description = \"...\"` — it is the line the \
                     model reads in the deferred catalog to decide whether to load it",
                ));
            }
        } else if attrs.description.is_some() {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "`description` is only read when activation = deferred; set it or drop the \
                 description",
            ));
        }
        Ok(attrs)
    }
}

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as syn::Item);
    let attrs = parse_macro_input!(attr as AbilityAttrs);
    let runic = runic_root();

    let (ident, generics) = match &input {
        syn::Item::Struct(item) => (&item.ident, &item.generics),
        syn::Item::Enum(item) => (&item.ident, &item.generics),
        other => {
            return syn::Error::new_spanned(
                other,
                "#[ability] goes on the type that owns `async fn ability(&self, base: Ability, \
                 ctx: &BuildCtx<'_>) -> anyhow::Result<Ability>`, not on the impl block",
            )
            .to_compile_error()
            .into();
        }
    };

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let (name_method, descriptor_method) = match &attrs.id {
        Some(id) => {
            let descriptor = if attrs.deferred {
                let description = attrs.description.clone().unwrap();
                quote!(#runic::ability::AbilityDescriptor::deferred(#id, #description))
            } else {
                quote!(#runic::ability::AbilityDescriptor {
                    id: Some(#id.to_string()),
                    description: None,
                    activation: #runic::ability::ActivationPolicy::Eager,
                })
            };
            (
                quote! { fn name(&self) -> &str { #id } },
                quote! {
                    fn descriptor(&self) -> #runic::ability::AbilityDescriptor {
                        #descriptor
                    }
                },
            )
        }
        None => (quote! {}, quote! {}),
    };

    let output = quote! {
        #input

        #[#runic::__private::async_trait]
        impl #impl_generics #runic::ability::ToAbility for #ident #ty_generics #where_clause {
            #name_method
            #descriptor_method

            async fn to_ability(
                &self,
                base: #runic::ability::Ability,
                ctx: &#runic::ability::BuildCtx<'_>,
            ) -> #runic::__private::anyhow::Result<#runic::ability::Ability> {
                self.ability(base, ctx).await
            }
        }
    };
    output.into()
}
