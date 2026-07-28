use proc_macro::TokenStream;
use quote::quote;
use syn::{Meta, parse_macro_input};

use crate::shared::{doc_description, snake_case};

struct ToolAttrs {
    name: Option<String>,
    description: Option<String>,
    args: Option<syn::Path>,
    parallelizable: bool,
}

impl syn::parse::Parse for ToolAttrs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut attrs = ToolAttrs {
            name: None,
            description: None,
            args: None,
            parallelizable: false,
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
                    "expected `key = value`; tool takes name, description, args, execution",
                ));
            };
            let key = nv
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_default();

            if key == "execution" {
                let syn::Expr::Path(p) = &nv.value else {
                    return Err(syn::Error::new_spanned(
                        &nv.value,
                        "expected `serial` or `parallel`",
                    ));
                };
                let ident = p
                    .path
                    .get_ident()
                    .ok_or_else(|| syn::Error::new_spanned(&nv.value, "expected an identifier"))?;
                attrs.parallelizable = match ident.to_string().as_str() {
                    "serial" => false,
                    "parallel" => true,
                    other => {
                        return Err(syn::Error::new_spanned(
                            &nv.value,
                            format!("unknown execution `{other}`; expected serial or parallel"),
                        ));
                    }
                };
                continue;
            }

            if key == "args" {
                let syn::Expr::Path(p) = &nv.value else {
                    return Err(syn::Error::new_spanned(
                        &nv.value,
                        "`args` must be the type the model's arguments deserialize into, e.g. \
                         `args = AddArgs`",
                    ));
                };
                attrs.args = Some(p.path.clone());
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
                "name" => attrs.name = Some(s.value()),
                "description" => attrs.description = Some(s.value()),
                other => {
                    return Err(syn::Error::new_spanned(
                        &nv.path,
                        format!(
                            "unknown tool attribute `{other}`; expected name, description, \
                             args, execution"
                        ),
                    ));
                }
            }
        }
        Ok(attrs)
    }
}

pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as syn::Item);
    let attrs = parse_macro_input!(attr as ToolAttrs);

    let (ident, generics, item_attrs) = match &input {
        syn::Item::Struct(item) => (&item.ident, &item.generics, &item.attrs),
        syn::Item::Enum(item) => (&item.ident, &item.generics, &item.attrs),
        other => {
            return syn::Error::new_spanned(
                other,
                "#[tool] goes on the type that owns `async fn tool(&self, ...) -> \
                 anyhow::Result<ToolResult>`, not on the impl block",
            )
            .to_compile_error()
            .into();
        }
    };

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let tool_name = attrs.name.unwrap_or_else(|| snake_case(&ident.to_string()));
    let description = attrs
        .description
        .or_else(|| doc_description(item_attrs))
        .unwrap_or_else(|| tool_name.replace('_', " "));

    let (schema_body, execute_body) = match &attrs.args {
        Some(args_ty) => (
            quote! {
                let mut schema = ::runic::__private::serde_json::to_value(
                    schemars::schema_for!(#args_ty)
                ).unwrap_or_default();
                if let Some(object) = schema.as_object_mut() {
                    object.remove("$schema");
                    object.remove("title");
                }
                schema
            },
            quote! {
                let typed_args: #args_ty = match ::runic::__private::serde_json::from_value(args) {
                    Ok(typed_args) => typed_args,
                    Err(error) => {
                        return Ok(::runic::tool::ToolResult::error(format!(
                            "invalid arguments for `{}`: {error}", #tool_name
                        )));
                    }
                };
                self.tool(typed_args, ctx).await
            },
        ),
        None => (
            quote!(::runic::__private::serde_json::json!({ "type": "object" })),
            quote! {
                let _ = args;
                self.tool(ctx).await
            },
        ),
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
        #input

        #[::runic::__private::async_trait]
        impl #impl_generics ::runic::tool::Tool for #ident #ty_generics #where_clause {
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
