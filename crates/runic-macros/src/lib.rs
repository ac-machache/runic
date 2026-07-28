use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{FnArg, ItemFn, Meta, parse_macro_input};

enum ParamKind {
    Context,
    Args(Box<syn::Type>),
}

struct ToolAttrs {
    parallelizable: bool,
}

impl syn::parse::Parse for ToolAttrs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut attrs = ToolAttrs {
            parallelizable: false,
        };
        let punctuated =
            syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated(input)?;
        for meta in punctuated {
            match &meta {
                Meta::Path(path) if path.is_ident("parallelizable") => {
                    attrs.parallelizable = true;
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "unknown tool attribute; expected `parallelizable`",
                    ));
                }
            }
        }
        Ok(attrs)
    }
}

fn doc_description(input_fn: &ItemFn) -> String {
    let doc_lines: Vec<String> = input_fn
        .attrs
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
    if doc_lines.is_empty() {
        input_fn.sig.ident.to_string().replace('_', " ")
    } else {
        doc_lines.join(" ")
    }
}

fn pascal_case(name: &str) -> String {
    name.split('_')
        .map(|segment| {
            let mut chars = segment.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => first.to_uppercase().to_string() + chars.as_str(),
            }
        })
        .collect()
}

fn classify_params(input_fn: &ItemFn) -> syn::Result<Vec<ParamKind>> {
    let mut params = Vec::new();
    let mut args_seen = false;
    for input in &input_fn.sig.inputs {
        let FnArg::Typed(pat_type) = input else {
            return Err(syn::Error::new_spanned(
                input,
                "#[tool] functions cannot take `self`",
            ));
        };
        let type_tokens = &pat_type.ty;
        let type_text = quote!(#type_tokens).to_string();
        if type_text.contains("ToolContext") {
            params.push(ParamKind::Context);
        } else {
            if args_seen {
                return Err(syn::Error::new_spanned(
                    pat_type,
                    "#[tool] functions take at most one non-context parameter; \
                     group the arguments into one struct deriving `serde::Deserialize` and `schemars::JsonSchema`",
                ));
            }
            args_seen = true;
            params.push(ParamKind::Args(pat_type.ty.clone()));
        }
    }
    Ok(params)
}

#[proc_macro_attribute]
pub fn tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input_fn = parse_macro_input!(item as ItemFn);
    let attrs = if attr.is_empty() {
        ToolAttrs {
            parallelizable: false,
        }
    } else {
        parse_macro_input!(attr as ToolAttrs)
    };

    if input_fn.sig.asyncness.is_none() {
        return syn::Error::new_spanned(input_fn.sig.fn_token, "#[tool] functions must be async")
            .to_compile_error()
            .into();
    }

    let params = match classify_params(&input_fn) {
        Ok(params) => params,
        Err(error) => return error.to_compile_error().into(),
    };

    let fn_name = &input_fn.sig.ident;
    let fn_vis = &input_fn.vis;
    let tool_name = fn_name.to_string();
    let description = doc_description(&input_fn);
    let struct_name = format_ident!("{}", pascal_case(&tool_name));

    let args_type = params.iter().find_map(|param| match param {
        ParamKind::Args(ty) => Some(ty.clone()),
        ParamKind::Context => None,
    });

    let schema_body = match &args_type {
        Some(args_ty) => quote! {
            let mut schema = ::runic::__private::serde_json::to_value(
                schemars::schema_for!(#args_ty)
            ).unwrap_or_default();
            if let Some(object) = schema.as_object_mut() {
                object.remove("$schema");
                object.remove("title");
            }
            schema
        },
        None => quote! {
            ::runic::__private::serde_json::json!({ "type": "object" })
        },
    };

    let call_args: Vec<proc_macro2::TokenStream> = params
        .iter()
        .map(|param| match param {
            ParamKind::Context => quote!(ctx),
            ParamKind::Args(_) => quote!(typed_args),
        })
        .collect();

    let execute_body = match &args_type {
        Some(args_ty) => quote! {
            let typed_args: #args_ty = match ::runic::__private::serde_json::from_value(args) {
                Ok(typed_args) => typed_args,
                Err(error) => {
                    return Ok(::runic::tool::ToolResult::error(format!(
                        "invalid arguments for `{}`: {error}", #tool_name
                    )));
                }
            };
            #fn_name(#(#call_args),*).await
        },
        None => quote! {
            let _ = args;
            #fn_name(#(#call_args),*).await
        },
    };

    let parallelizable_override = if attrs.parallelizable {
        quote! {
            fn parallelizable(&self) -> bool {
                true
            }
        }
    } else {
        quote! {}
    };

    let output = quote! {
        #input_fn

        #fn_vis struct #struct_name;

        #[::runic::__private::async_trait]
        impl ::runic::tool::Tool for #struct_name {
            fn name(&self) -> &str {
                #tool_name
            }

            fn description(&self) -> &str {
                #description
            }

            fn parameters_schema(&self) -> ::runic::__private::serde_json::Value {
                #schema_body
            }

            #parallelizable_override

            async fn execute(
                &self,
                args: ::runic::__private::serde_json::Value,
                ctx: &::runic::tool::ToolContext,
            ) -> ::runic::__private::anyhow::Result<::runic::tool::ToolResult> {
                #execute_body
            }
        }
    };

    output.into()
}

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

#[proc_macro_attribute]
pub fn hook(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input_fn = parse_macro_input!(item as ItemFn);
    let attrs = parse_macro_input!(attr as HookAttrs);

    if input_fn.sig.asyncness.is_none() {
        return syn::Error::new_spanned(input_fn.sig.fn_token, "#[hook] functions must be async")
            .to_compile_error()
            .into();
    }

    let fn_name = &input_fn.sig.ident;
    let fn_vis = &input_fn.vis;
    let struct_name = format_ident!("{}", pascal_case(&fn_name.to_string()));
    let hook_name = attrs.name.unwrap_or_else(|| fn_name.to_string());
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
            quote!(#fn_name(state, call)),
        ),
        (HookPoint::BeforeTool, HookKind::Write) => (
            quote!(state: #state_ty, call: &mut ::runic::types::ToolCall),
            quote!(#fn_name(state, call)),
        ),
        (HookPoint::AfterTool, _) => (
            quote!(
                state: #state_ty,
                call: &::runic::types::ToolCall,
                result: &::runic::tool::ToolResult
            ),
            quote!(#fn_name(state, call, result)),
        ),
        _ => (quote!(state: #state_ty), quote!(#fn_name(state))),
    };

    let output = quote! {
        #input_fn

        #fn_vis struct #struct_name;

        #[::runic::__private::async_trait]
        impl #trait_path for #struct_name {
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
