use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Meta, parse_macro_input};

struct HookAttrs {
    kind: HookKind,
    at: HookPoint,
    name: Option<String>,
    priority: Option<i32>,
}

#[derive(Clone, Copy, PartialEq)]
enum HookKind {
    Read,
    Write,
}

#[derive(Clone, Copy)]
enum HookPoint {
    BeforeAgent,
    BeforeModel,
    BeforeTool,
    AfterTool,
    AfterModel,
    AfterAgent,
}

impl HookPoint {
    fn parse(name: &str, span: proc_macro2::Span) -> syn::Result<Self> {
        Ok(match name {
            "before_agent" => Self::BeforeAgent,
            "before_model" => Self::BeforeModel,
            "before_tool" => Self::BeforeTool,
            "after_tool" => Self::AfterTool,
            "after_model" => Self::AfterModel,
            "after_agent" => Self::AfterAgent,
            other => {
                return Err(syn::Error::new(
                    span,
                    format!(
                        "unknown hook point `{other}`; expected one of before_agent, \
                         before_model, before_tool, after_tool, after_model, after_agent"
                    ),
                ));
            }
        })
    }

    fn method(self) -> proc_macro2::Ident {
        let name = match self {
            Self::BeforeAgent => "before_agent",
            Self::BeforeModel => "before_model",
            Self::BeforeTool => "before_tool",
            Self::AfterTool => "after_tool",
            Self::AfterModel => "after_model",
            Self::AfterAgent => "after_agent",
        };
        format_ident!("{}", name)
    }

    fn variant(self) -> proc_macro2::Ident {
        let name = match self {
            Self::BeforeAgent => "BeforeAgent",
            Self::BeforeModel => "BeforeModel",
            Self::BeforeTool => "BeforeTool",
            Self::AfterTool => "AfterTool",
            Self::AfterModel => "AfterModel",
            Self::AfterAgent => "AfterAgent",
        };
        format_ident!("{}", name)
    }
}

impl syn::parse::Parse for HookAttrs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut kind = None;
        let mut at = None;
        let mut name = None;
        let mut priority = None;
        let punctuated =
            syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated(input)?;
        for meta in punctuated {
            let Meta::NameValue(nv) = &meta else {
                return Err(syn::Error::new_spanned(
                    &meta,
                    "expected `key = value`; hook takes kind, at, name, priority",
                ));
            };
            let key = nv
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_default();
            match key.as_str() {
                "kind" => {
                    let syn::Expr::Path(p) = &nv.value else {
                        return Err(syn::Error::new_spanned(&nv.value, "expected read or write"));
                    };
                    let ident = p
                        .path
                        .get_ident()
                        .map(|i| i.to_string())
                        .unwrap_or_default();
                    kind = Some(match ident.as_str() {
                        "read" => HookKind::Read,
                        "write" => HookKind::Write,
                        other => {
                            return Err(syn::Error::new_spanned(
                                &nv.value,
                                format!("unknown hook kind `{other}`; expected read or write"),
                            ));
                        }
                    });
                }
                "at" => {
                    let syn::Expr::Path(p) = &nv.value else {
                        return Err(syn::Error::new_spanned(&nv.value, "expected a hook point"));
                    };
                    let ident = p.path.get_ident().ok_or_else(|| {
                        syn::Error::new_spanned(&nv.value, "expected a hook point")
                    })?;
                    at = Some(HookPoint::parse(&ident.to_string(), ident.span())?);
                }
                "name" => {
                    let syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(s),
                        ..
                    }) = &nv.value
                    else {
                        return Err(syn::Error::new_spanned(&nv.value, "name must be a string"));
                    };
                    name = Some(s.value());
                }
                "priority" => {
                    let syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Int(i),
                        ..
                    }) = &nv.value
                    else {
                        return Err(syn::Error::new_spanned(
                            &nv.value,
                            "priority must be an integer",
                        ));
                    };
                    priority = Some(i.base10_parse::<i32>()?);
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        &nv.path,
                        format!(
                            "unknown hook attribute `{other}`; expected kind, at, name, priority"
                        ),
                    ));
                }
            }
        }
        let Some(kind) = kind else {
            return Err(input.error("hook requires `kind = read` or `kind = write`"));
        };
        let Some(at) = at else {
            return Err(input.error("hook requires `at = <point>`"));
        };
        Ok(HookAttrs {
            kind,
            at,
            name,
            priority,
        })
    }
}

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as syn::Item);
    let attrs = parse_macro_input!(attr as HookAttrs);

    let (ident, generics) = match &input {
        syn::Item::Struct(item) => (&item.ident, &item.generics),
        syn::Item::Enum(item) => (&item.ident, &item.generics),
        other => {
            return syn::Error::new_spanned(
                other,
                "#[hook] goes on the type that owns `async fn hook(&self, ...)`, not on the \
                 impl block",
            )
            .to_compile_error()
            .into();
        }
    };

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let hook_name = attrs.name.unwrap_or_else(|| ident.to_string());
    let priority = attrs.priority.unwrap_or(0);
    let method = attrs.at.method();
    let variant = attrs.at.variant();

    let (trait_path, outcome) = match attrs.kind {
        HookKind::Read => (
            quote!(::runic::hook::ReadHook),
            quote!(::runic::hook::HookSignal),
        ),
        HookKind::Write => (
            quote!(::runic::hook::WriteHook),
            quote!(::runic::hook::HookOutcome),
        ),
    };

    let state_ty = match attrs.kind {
        HookKind::Read => quote!(&::runic::state::AgentState),
        HookKind::Write => quote!(&mut ::runic::state::AgentState),
    };

    let (params, forward) = match (attrs.at, attrs.kind) {
        (HookPoint::BeforeTool, HookKind::Read) => (
            quote!(state: #state_ty, call: &::runic::types::ToolCall),
            quote!(self.hook(state, call)),
        ),
        (HookPoint::BeforeTool, HookKind::Write) => (
            quote!(state: #state_ty, call: &mut ::runic::types::ToolCall),
            quote!(self.hook(state, call)),
        ),
        (HookPoint::AfterTool, _) => (
            quote!(
                state: #state_ty,
                call: &::runic::types::ToolCall,
                result: &::runic::tool::ToolResult
            ),
            quote!(self.hook(state, call, result)),
        ),
        _ => (quote!(state: #state_ty), quote!(self.hook(state))),
    };

    let output = quote! {
        #input

        #[::runic::__private::async_trait]
        impl #impl_generics #trait_path for #ident #ty_generics #where_clause {
            fn name(&self) -> &str {
                #hook_name
            }

            fn priority(&self) -> i32 {
                #priority
            }

            fn points(&self) -> &'static [::runic::hook::HookLifecycle] {
                &[::runic::hook::HookLifecycle::#variant]
            }

            async fn #method(&self, #params) -> #outcome {
                #forward.await
            }
        }
    };
    output.into()
}
