extern crate proc_macro;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse_quote;
use syn::spanned::Spanned;
use syn::{Error, Ident, Result};

/// Turn a node into a class.
#[proc_macro_attribute]
pub fn class(_: TokenStream, item: TokenStream) -> TokenStream {
    let impl_block = syn::parse_macro_input!(item as syn::ItemImpl);
    expand(impl_block).unwrap_or_else(|err| err.to_compile_error()).into()
}

/// Expand an impl block for a node.
fn expand(mut impl_block: syn::ItemImpl) -> Result<TokenStream2> {
    // Split the node type into name and generic type arguments.
    let params = &impl_block.generics.params;
    let self_ty = &*impl_block.self_ty;
    let (self_name, self_args) = parse_self(self_ty)?;

    let module = quote::format_ident!("{}_types", self_name);

    let mut key_modules = vec![];
    let mut properties = vec![];
    let mut construct = None;
    let mut set = None;

    for item in std::mem::take(&mut impl_block.items) {
        match item {
            syn::ImplItem::Const(mut item) => {
                let (property, module) =
                    process_const(&mut item, params, self_ty, &self_name, &self_args)?;
                properties.push(property);
                key_modules.push(module);
                impl_block.items.push(syn::ImplItem::Const(item));
            }
            syn::ImplItem::Method(method) => {
                match method.sig.ident.to_string().as_str() {
                    "construct" => construct = Some(method),
                    "set" => set = Some(method),
                    _ => return Err(Error::new(method.span(), "unexpected method")),
                }
            }
            _ => return Err(Error::new(item.span(), "unexpected item")),
        }
    }

    let construct =
        construct.ok_or_else(|| Error::new(impl_block.span(), "missing constructor"))?;

    let set = set.unwrap_or_else(|| {
        let sets = properties.into_iter().filter(|p| !p.skip).map(|property| {
            let name = property.name;
            let string = name.to_string().replace("_", "-").to_lowercase();

            let alternative = if property.variadic {
                quote! {
                    .or_else(|| {
                        let list: Vec<_> = args.all().collect();
                        (!list.is_empty()).then(|| list)
                    })
                }
            } else if property.shorthand {
                quote! { .or_else(|| args.find()) }
            } else {
                quote! {}
            };

            quote! { styles.set_opt(Self::#name, args.named(#string)? #alternative); }
        });

        parse_quote! {
            fn set(args: &mut Args, styles: &mut StyleMap) -> TypResult<()> {
                #(#sets)*
                Ok(())
            }
        }
    });

    // Put everything into a module with a hopefully unique type to isolate
    // it from the outside.
    Ok(quote! {
        #[allow(non_snake_case)]
        mod #module {
            use std::any::TypeId;
            use std::marker::PhantomData;
            use once_cell::sync::Lazy;
            use crate::eval::{Construct, Nonfolding, Property, Set};
            use super::*;

            #impl_block

            impl<#params> Construct for #self_ty {
                #construct
            }

            impl<#params> Set for #self_ty {
                #set
            }

            #(#key_modules)*
        }
    })
}

/// A style property.
struct Property {
    name: Ident,
    shorthand: bool,
    variadic: bool,
    skip: bool,
}

/// Parse the name and generic type arguments of the node type.
fn parse_self(self_ty: &syn::Type) -> Result<(String, Vec<&syn::Type>)> {
    // Extract the node type for which we want to generate properties.
    let path = match self_ty {
        syn::Type::Path(path) => path,
        ty => return Err(Error::new(ty.span(), "must be a path type")),
    };

    // Split up the type into its name and its generic type arguments.
    let last = path.path.segments.last().unwrap();
    let self_name = last.ident.to_string();
    let self_args = match &last.arguments {
        syn::PathArguments::AngleBracketed(args) => args
            .args
            .iter()
            .filter_map(|arg| match arg {
                syn::GenericArgument::Type(ty) => Some(ty),
                _ => None,
            })
            .collect(),
        _ => vec![],
    };

    Ok((self_name, self_args))
}

/// Process a single const item.
fn process_const(
    item: &mut syn::ImplItemConst,
    params: &syn::punctuated::Punctuated<syn::GenericParam, syn::Token![,]>,
    self_ty: &syn::Type,
    self_name: &str,
    self_args: &[&syn::Type],
) -> Result<(Property, syn::ItemMod)> {
    // The module that will contain the `Key` type.
    let module_name = &item.ident;

    // The type of the property's value is what the user of our macro wrote
    // as type of the const ...
    let value_ty = &item.ty;

    // ... but the real type of the const becomes Key<#key_args>.
    let key_args = quote! { #value_ty #(, #self_args)* };

    // The display name, e.g. `TextNode::STRONG`.
    let name = format!("{}::{}", self_name, &item.ident);

    // The default value of the property is what the user wrote as
    // initialization value of the const.
    let default = &item.expr;

    let mut folder = None;
    let mut property = Property {
        name: item.ident.clone(),
        shorthand: false,
        variadic: false,
        skip: false,
    };

    for attr in std::mem::take(&mut item.attrs) {
        if attr.path.is_ident("fold") {
            // Look for a folding function like `#[fold(u64::add)]`.
            let func: syn::Expr = attr.parse_args()?;
            folder = Some(quote! {
                const FOLDING: bool = true;

                fn fold(inner: Self::Value, outer: Self::Value) -> Self::Value {
                    let f: fn(Self::Value, Self::Value) -> Self::Value = #func;
                    f(inner, outer)
                }
            });
        } else if attr.path.is_ident("shorthand") {
            property.shorthand = true;
        } else if attr.path.is_ident("variadic") {
            property.variadic = true;
        } else if attr.path.is_ident("skip") {
            property.skip = true;
        } else {
            item.attrs.push(attr);
        }
    }

    if property.shorthand && property.variadic {
        return Err(Error::new(
            property.name.span(),
            "shorthand and variadic are mutually exclusive",
        ));
    }

    let nonfolding = folder.is_none().then(|| {
        quote! {
            impl<#params> Nonfolding for Key<#key_args> {}
        }
    });

    // Generate the module code.
    let module = parse_quote! {
        #[allow(non_snake_case)]
        mod #module_name {
            use super::*;

            pub struct Key<VALUE, #params>(pub PhantomData<(VALUE, #key_args)>);

            impl<#params> Copy for Key<#key_args> {}
            impl<#params> Clone for Key<#key_args> {
                fn clone(&self) -> Self {
                    *self
                }
            }

            impl<#params> Property for Key<#key_args> {
                type Value = #value_ty;

                const NAME: &'static str = #name;

                fn node_id() -> TypeId {
                    TypeId::of::<#self_ty>()
                }

                fn default() -> Self::Value {
                    #default
                }

                fn default_ref() -> &'static Self::Value {
                    static LAZY: Lazy<#value_ty> = Lazy::new(|| #default);
                    &*LAZY
                }

                #folder
            }

            #nonfolding
        }
    };

    // Replace type and initializer expression with the `Key`.
    item.attrs.retain(|attr| !attr.path.is_ident("fold"));
    item.ty = parse_quote! { #module_name::Key<#key_args> };
    item.expr = parse_quote! { #module_name::Key(PhantomData) };

    Ok((property, module))
}