use proc_macro::TokenStream;
use quote::quote;
use syn::{Meta, parse_macro_input};

use crate::shared::{runic_root, string_value};

struct AgentAttrs {
    name: String,
    description: Option<String>,
}

impl syn::parse::Parse for AgentAttrs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut name = None;
        let mut description = None;

        let punctuated =
            syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated(input)?;
        for meta in punctuated {
            let Meta::NameValue(nv) = &meta else {
                return Err(syn::Error::new_spanned(
                    &meta,
                    "expected `key = value`; agent takes name, description",
                ));
            };
            let key = nv
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_default();

            match key.as_str() {
                "name" => name = Some(string_value(nv, &key)?),
                "description" => description = Some(string_value(nv, &key)?),
                "kind" => {
                    return Err(syn::Error::new_spanned(
                        &nv.path,
                        "`kind` is gone — use `#[agent]` for one a host serves by name and \
                         `#[subagent]` for a delegatable specialist",
                    ));
                }
                "model" | "invocation" => {
                    return Err(syn::Error::new_spanned(
                        &nv.path,
                        format!(
                            "`{key}` belongs to #[subagent] — a root agent has no parent to \
                             inherit a model from and nobody delegates to it, so it builds its \
                             own Llm in agent()"
                        ),
                    ));
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        &nv.path,
                        format!("unknown agent attribute `{other}`; expected name, description"),
                    ));
                }
            }
        }

        let Some(name) = name else {
            return Err(input.error(
                "agent requires `name = \"...\"` — it is the name a host resolves, the one a \
                 client sends as {\"agent\": \"...\"}",
            ));
        };

        Ok(AgentAttrs { name, description })
    }
}

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as syn::Item);
    let attrs = parse_macro_input!(attr as AgentAttrs);
    let runic = runic_root();

    let (ident, generics) = match &input {
        syn::Item::Struct(item) => (&item.ident, &item.generics),
        syn::Item::Enum(item) => (&item.ident, &item.generics),
        other => {
            return syn::Error::new_spanned(
                other,
                "#[agent] goes on the type that owns `async fn agent(&self) -> \
                 anyhow::Result<Agent>`, not on the impl block",
            )
            .to_compile_error()
            .into();
        }
    };

    let AgentAttrs { name, description } = attrs;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let description_method = match &description {
        Some(text) => quote! {
            fn description(&self) -> Option<&str> {
                Some(#text)
            }
        },
        None => quote! {},
    };

    let output = quote! {
        #input

        #[#runic::__private::async_trait]
        impl #impl_generics #runic::AgentDef for #ident #ty_generics #where_clause {
            fn name(&self) -> &str {
                #name
            }

            #description_method

            async fn build_agent(&self) -> #runic::__private::anyhow::Result<#runic::Agent> {
                self.agent().await
            }
        }
    };
    output.into()
}
