use proc_macro::TokenStream;
use quote::quote;
use syn::{Meta, parse_macro_input};

use crate::shared::{ident_value, runic_root, string_value};

struct SubagentAttrs {
    name: String,
    description: String,
    model: Option<String>,
    invocation: Option<String>,
}

impl syn::parse::Parse for SubagentAttrs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut name = None;
        let mut description = None;
        let mut model = None;
        let mut invocation = None;

        let punctuated =
            syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated(input)?;
        for meta in punctuated {
            let Meta::NameValue(nv) = &meta else {
                return Err(syn::Error::new_spanned(
                    &meta,
                    "expected `key = value`; subagent takes name, description, model, invocation",
                ));
            };
            let key = nv
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_default();

            match key.as_str() {
                "invocation" => {
                    let ident = ident_value(nv, "expected `any`, `sync` or `background`")?;
                    let value = ident.to_string();
                    if !matches!(value.as_str(), "any" | "sync" | "background") {
                        return Err(syn::Error::new_spanned(
                            &nv.value,
                            format!(
                                "unknown invocation `{value}`; expected any, sync or background"
                            ),
                        ));
                    }
                    invocation = Some(value);
                }
                "name" => name = Some(string_value(nv, &key)?),
                "description" => description = Some(string_value(nv, &key)?),
                "model" => model = Some(string_value(nv, &key)?),
                "kind" => {
                    return Err(syn::Error::new_spanned(
                        &nv.path,
                        "`kind` is gone — use `#[subagent]` for a delegatable specialist and \
                         `#[agent]` for one a host serves by name",
                    ));
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        &nv.path,
                        format!(
                            "unknown subagent attribute `{other}`; expected name, description, \
                             model, invocation"
                        ),
                    ));
                }
            }
        }

        let Some(name) = name else {
            return Err(input.error(
                "subagent requires `name = \"...\"` — it is how a parent addresses this agent \
                 in the delegate roster",
            ));
        };
        let Some(description) = description else {
            return Err(input.error(
                "subagent requires `description = \"...\"` — it is the roster line a parent's \
                 model reads when choosing whom to delegate to. This agent's own prompt goes in \
                 agent() via Llm::instructions",
            ));
        };

        Ok(SubagentAttrs {
            name,
            description,
            model,
            invocation,
        })
    }
}

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as syn::Item);
    let attrs = parse_macro_input!(attr as SubagentAttrs);
    let runic = runic_root();

    let (ident, generics) = match &input {
        syn::Item::Struct(item) => (&item.ident, &item.generics),
        syn::Item::Enum(item) => (&item.ident, &item.generics),
        other => {
            return syn::Error::new_spanned(
                other,
                "#[subagent] goes on the type that owns `async fn agent(&self, llm: Llm) -> \
                 anyhow::Result<Agent>`, not on the impl block",
            )
            .to_compile_error()
            .into();
        }
    };

    let SubagentAttrs {
        name,
        description,
        model,
        invocation,
    } = attrs;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let model_expr = match &model {
        Some(model) => quote!(#model),
        None => quote!(ctx.model),
    };

    // Only a declared model can be contradicted; an inherited one is whatever
    // the parent had, so there is nothing to disagree with.
    let model_check = match &model {
        Some(model) => quote! {
            if agent.model() != #model {
                return Err(#runic::__private::anyhow::Error::msg(format!(
                    "subagent `{}` declares model = `{}`, but agent() returned one built on \
                     `{}`. Use the `llm` you were handed, or drop the model attribute.",
                    #name, #model, agent.model()
                )));
            }
        },
        None => quote! {},
    };

    let invocation_expr = match invocation.as_deref() {
        Some("sync") => quote!(#runic::subagent::Invocation::Sync),
        Some("background") => quote!(#runic::subagent::Invocation::Background),
        _ => quote!(#runic::subagent::Invocation::Any),
    };

    let output = quote! {
        #input

        #[#runic::__private::async_trait::async_trait]
        impl #impl_generics #runic::ability::ToAbility for #ident #ty_generics #where_clause {
            fn name(&self) -> &str {
                #name
            }

            async fn to_ability(
                &self,
                base: #runic::ability::Ability,
                ctx: &#runic::ability::BuildCtx<'_>,
            ) -> #runic::__private::anyhow::Result<#runic::ability::Ability> {
                let llm = #runic::Llm::new(ctx.provider.clone(), #model_expr);
                let agent = self.agent(llm).await?;
                #model_check
                Ok(base.subagent(
                    #runic::subagent::Subagent::new(#name, #description, agent)
                        .invocation(#invocation_expr),
                ))
            }
        }
    };
    output.into()
}
