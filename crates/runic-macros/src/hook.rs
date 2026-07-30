use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Meta, parse_macro_input};

use crate::shared::{dep_path, hook_path, provider_path, state_path, tool_path, types_path};

struct HookAttrs {
    kind: HookKind,
    at: HookPoints,
    name: Option<String>,
    priority: Option<i32>,
}

#[derive(Clone, Copy, PartialEq)]
enum HookKind {
    Read,
    Write,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HookPoint {
    BeforeAgent,
    BeforeModel,
    BeforeTool,
    AfterTool,
    AfterModel,
    AfterAgent,
}

impl HookPoint {
    const ALL: [Self; 6] = [
        Self::BeforeAgent,
        Self::BeforeModel,
        Self::BeforeTool,
        Self::AfterTool,
        Self::AfterModel,
        Self::AfterAgent,
    ];

    fn snake(self) -> &'static str {
        match self {
            Self::BeforeAgent => "before_agent",
            Self::BeforeModel => "before_model",
            Self::BeforeTool => "before_tool",
            Self::AfterTool => "after_tool",
            Self::AfterModel => "after_model",
            Self::AfterAgent => "after_agent",
        }
    }

    fn parse(name: &str, span: proc_macro2::Span) -> syn::Result<Self> {
        Self::ALL
            .into_iter()
            .find(|point| point.snake() == name)
            .ok_or_else(|| {
                let known = Self::ALL.map(Self::snake).join(", ");
                syn::Error::new(
                    span,
                    format!("unknown hook point `{name}`; expected one of {known}"),
                )
            })
    }

    fn method(self) -> proc_macro2::Ident {
        format_ident!("{}", self.snake())
    }

    fn variant(self) -> proc_macro2::Ident {
        let pascal: String = self
            .snake()
            .split('_')
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect();
        format_ident!("{}", pascal)
    }
}

#[derive(Clone, Copy, Default)]
struct HookPoints([bool; 6]);

impl HookPoints {
    fn insert(&mut self, point: HookPoint) {
        let slot = HookPoint::ALL.iter().position(|p| *p == point).unwrap();
        self.0[slot] = true;
    }

    fn is_empty(self) -> bool {
        self.0.iter().all(|set| !set)
    }

    fn len(self) -> usize {
        self.0.iter().filter(|set| **set).count()
    }

    fn iter(self) -> impl Iterator<Item = HookPoint> {
        HookPoint::ALL
            .into_iter()
            .enumerate()
            .filter_map(move |(index, point)| self.0[index].then_some(point))
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
                    let listed = match &nv.value {
                        syn::Expr::Array(array) => array.elems.iter().collect::<Vec<_>>(),
                        single => vec![single],
                    };
                    let mut points = HookPoints::default();
                    for expr in listed {
                        let syn::Expr::Path(path) = expr else {
                            return Err(syn::Error::new_spanned(expr, "expected a hook point"));
                        };
                        let ident = path.path.get_ident().ok_or_else(|| {
                            syn::Error::new_spanned(expr, "expected a hook point")
                        })?;
                        points.insert(HookPoint::parse(&ident.to_string(), ident.span())?);
                    }
                    if points.is_empty() {
                        return Err(syn::Error::new_spanned(
                            &nv.value,
                            "`at` needs at least one hook point",
                        ));
                    }
                    at = Some(points);
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
    let hook = hook_path();
    let state = state_path();
    let types = types_path();
    let tool = tool_path();
    let provider = provider_path();
    let async_trait = dep_path("async-trait");

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

    let (trait_path, outcome) = match attrs.kind {
        HookKind::Read => (quote!(#hook::ReadHook), quote!(#hook::HookSignal)),
        HookKind::Write => (quote!(#hook::WriteHook), quote!(#hook::HookOutcome)),
    };

    let state_ty = match attrs.kind {
        HookKind::Read => quote!(&#state::AgentState),
        HookKind::Write => quote!(&mut #state::AgentState),
    };

    let variants = attrs.at.iter().map(HookPoint::variant);
    let one_point = attrs.at.len() == 1;

    let methods = attrs.at.iter().map(|point| {
        let method = point.method();
        let body = if one_point {
            format_ident!("hook")
        } else {
            point.method()
        };
        let (params, forward) = match (point, attrs.kind) {
            (HookPoint::BeforeTool, HookKind::Read) => (
                quote!(state: #state_ty, call: &#types::ToolCall),
                quote!(self.#body(state, call)),
            ),
            (HookPoint::BeforeTool, HookKind::Write) => (
                quote!(state: #state_ty, call: &mut #types::ToolCall),
                quote!(self.#body(state, call)),
            ),
            (HookPoint::AfterTool, _) => (
                quote!(
                    state: #state_ty,
                    call: &#types::ToolCall,
                    result: &#tool::ToolResult
                ),
                quote!(self.#body(state, call, result)),
            ),
            (HookPoint::AfterModel, HookKind::Read) => (
                quote!(state: #state_ty, response: &#provider::CompletionResponse),
                quote!(self.#body(state, response)),
            ),
            (HookPoint::AfterModel, HookKind::Write) => (
                quote!(state: #state_ty, response: &mut #provider::CompletionResponse),
                quote!(self.#body(state, response)),
            ),
            (HookPoint::BeforeModel, HookKind::Read) => (
                quote!(state: #state_ty, request: &#provider::CompletionRequest),
                quote!(self.#body(state, request)),
            ),
            (HookPoint::BeforeModel, HookKind::Write) => (
                quote!(state: #state_ty, request: &mut #provider::CompletionRequest),
                quote!(self.#body(state, request)),
            ),
            _ => (quote!(state: #state_ty), quote!(self.#body(state))),
        };
        quote! {
            async fn #method(&self, #params) -> #outcome {
                #forward.await
            }
        }
    });

    let output = quote! {
        #input

        #[#async_trait::async_trait]
        impl #impl_generics #trait_path for #ident #ty_generics #where_clause {
            fn name(&self) -> &str {
                #hook_name
            }

            fn priority(&self) -> i32 {
                #priority
            }

            fn points(&self) -> &'static [#hook::HookLifecycle] {
                &[#(#hook::HookLifecycle::#variants),*]
            }

            #(#methods)*
        }
    };
    output.into()
}
