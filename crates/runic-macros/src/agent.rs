use proc_macro::TokenStream;
use quote::quote;
use syn::{Meta, parse_macro_input};

#[derive(PartialEq)]
enum Kind {
    Agent,
    Subagent,
}

struct AgentAttrs {
    kind: Kind,
    name: Option<String>,
    description: Option<String>,
    model: Option<String>,
    invocation: Option<String>,
}

fn ident_value<'a>(nv: &'a syn::MetaNameValue, expected: &str) -> syn::Result<&'a syn::Ident> {
    let syn::Expr::Path(p) = &nv.value else {
        return Err(syn::Error::new_spanned(&nv.value, expected.to_string()));
    };
    p.path
        .get_ident()
        .ok_or_else(|| syn::Error::new_spanned(&nv.value, expected.to_string()))
}

impl syn::parse::Parse for AgentAttrs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut kind = None;
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
                    "expected `key = value`; agent takes kind, name, description, model, \
                     invocation",
                ));
            };
            let key = nv
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_default();

            match key.as_str() {
                "kind" => {
                    let ident = ident_value(nv, "expected `agent` or `subagent`")?;
                    kind = Some(match ident.to_string().as_str() {
                        "agent" => Kind::Agent,
                        "subagent" => Kind::Subagent,
                        other => {
                            return Err(syn::Error::new_spanned(
                                &nv.value,
                                format!("unknown kind `{other}`; expected agent or subagent"),
                            ));
                        }
                    });
                }
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
                _ => {
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
                        "name" => name = Some(s.value()),
                        "description" => description = Some(s.value()),
                        "model" => model = Some(s.value()),
                        other => {
                            return Err(syn::Error::new_spanned(
                                &nv.path,
                                format!(
                                    "unknown agent attribute `{other}`; expected kind, name, \
                                     description, model, invocation"
                                ),
                            ));
                        }
                    }
                }
            }
        }

        let Some(kind) = kind else {
            return Err(input.error("agent requires `kind = agent` or `kind = subagent`"));
        };

        if kind == Kind::Agent {
            if name.is_some() || description.is_some() {
                return Err(input.error(
                    "kind = agent takes no name or description — those exist only so a parent \
                     can address a child in the delegate roster. Use kind = subagent",
                ));
            }
            if invocation.is_some() {
                return Err(input.error(
                    "kind = agent takes no invocation — nobody delegates to a root agent. \
                     Use kind = subagent",
                ));
            }
        } else {
            if name.is_none() {
                return Err(input.error(
                    "kind = subagent requires `name = \"...\"` — it is how a parent addresses \
                     this agent in the delegate roster",
                ));
            }
            if description.is_none() {
                return Err(input.error(
                    "kind = subagent requires `description = \"...\"` — it is the roster line \
                     a parent's model reads when choosing whom to delegate to. This agent's \
                     own prompt goes in agent() via Llm::instructions",
                ));
            }
        }

        Ok(AgentAttrs {
            kind,
            name,
            description,
            model,
            invocation,
        })
    }
}

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as syn::Item);
    let attrs = parse_macro_input!(attr as AgentAttrs);

    let (ident, generics) = match &input {
        syn::Item::Struct(item) => (&item.ident, &item.generics),
        syn::Item::Enum(item) => (&item.ident, &item.generics),
        other => {
            return syn::Error::new_spanned(
                other,
                "#[agent] goes on the type that owns `async fn agent(&self, llm: Llm) -> \
                 anyhow::Result<Agent>`, not on the impl block",
            )
            .to_compile_error()
            .into();
        }
    };

    if attrs.kind == Kind::Agent {
        return quote!(#input).into();
    }

    let name = attrs.name.unwrap();
    let description = attrs.description.unwrap();
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let model_expr = match &attrs.model {
        Some(model) => quote!(#model),
        None => quote!(ctx.model),
    };

    let invocation_expr = match attrs.invocation.as_deref() {
        Some("sync") => quote!(::runic::subagent::Invocation::Sync),
        Some("background") => quote!(::runic::subagent::Invocation::Background),
        _ => quote!(::runic::subagent::Invocation::Any),
    };

    let output = quote! {
        #input

        #[::runic::__private::async_trait]
        impl #impl_generics ::runic::ability::Ability for #ident #ty_generics #where_clause {
            fn name(&self) -> &str {
                #name
            }

            async fn contribute(
                &self,
                bundle: &mut ::runic::ability::AbilityBundle,
                ctx: &::runic::ability::BuildCtx<'_>,
            ) -> ::runic::__private::anyhow::Result<()> {
                let llm = ::runic::Llm::new(ctx.provider.clone(), #model_expr);
                let agent = self.agent(llm).await?;
                bundle.subagent(
                    ::runic::subagent::Subagent::new(#name, #description, agent)
                        .invocation(#invocation_expr),
                );
                Ok(())
            }
        }
    };
    output.into()
}
