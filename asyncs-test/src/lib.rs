extern crate proc_macro;

use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::Attribute;

type AttributeArgs = Punctuated<syn::Meta, syn::Token![,]>;

#[derive(Default)]
struct Configuration {
    crate_name: Option<Ident>,
    parallelism: Option<usize>,
}

impl Configuration {
    fn set_crate_name(&mut self, lit: &syn::Lit) -> Result<(), syn::Error> {
        let span = lit.span();
        if self.crate_name.is_some() {
            return Err(syn::Error::new(span, "crate name already set"));
        }
        if let syn::Lit::Str(s) = lit {
            if let Ok(path) = s.parse::<syn::Path>() {
                if let Some(ident) = path.get_ident() {
                    self.crate_name = Some(ident.clone());
                    return Ok(());
                }
            }
            return Err(syn::Error::new(span, format!("invalid crate name: {}", s.value())));
        }
        Err(syn::Error::new(span, "invalid crate name"))
    }

    fn set_parallelism(&mut self, lit: &syn::Lit) -> Result<(), syn::Error> {
        let span = lit.span();
        if self.parallelism.is_some() {
            return Err(syn::Error::new(span, "parallelism already set"));
        }
        if let syn::Lit::Int(lit) = lit {
            let parallelism = lit.base10_parse::<isize>()?;
            if parallelism >= 0 {
                self.parallelism = Some(parallelism as usize);
                return Ok(());
            }
        }
        Err(syn::Error::new(span, "parallelism should be non negative integer"))
    }
}

fn parse_config(args: AttributeArgs) -> Result<Configuration, syn::Error> {
    let mut config = Configuration::default();
    for arg in args {
        match arg {
            syn::Meta::NameValue(name_value) => {
                let name = name_value
                    .path
                    .get_ident()
                    .ok_or_else(|| syn::Error::new_spanned(&name_value, "invalid attribute name"))?
                    .to_string();
                let lit = match &name_value.value {
                    syn::Expr::Lit(syn::ExprLit { lit, .. }) => lit,
                    expr => return Err(syn::Error::new_spanned(expr, format!("{} expect literal value", name))),
                };
                match name.as_str() {
                    "parallelism" => config.set_parallelism(lit)?,
                    "crate" => config.set_crate_name(lit)?,
                    _ => return Err(syn::Error::new_spanned(&name_value, "unknown attribute name")),
                }
            },
            _ => return Err(syn::Error::new_spanned(arg, "unknown attribute")),
        }
    }
    Ok(config)
}

// Check whether given attribute is a test attribute of forms:
// * `#[test]`
// * `#[core::prelude::*::test]` or `#[::core::prelude::*::test]`
// * `#[std::prelude::*::test]` or `#[::std::prelude::*::test]`
fn is_test_attribute(attr: &Attribute) -> bool {
    let path = match &attr.meta {
        syn::Meta::Path(path) => path,
        _ => return false,
    };
    let candidates = [["core", "prelude", "*", "test"], ["std", "prelude", "*", "test"]];
    if path.leading_colon.is_none()
        && path.segments.len() == 1
        && path.segments[0].arguments.is_none()
        && path.segments[0].ident == "test"
    {
        return true;
    } else if path.segments.len() != candidates[0].len() {
        return false;
    }
    candidates.into_iter().any(|segments| {
        path.segments
            .iter()
            .zip(segments)
            .all(|(segment, path)| segment.arguments.is_none() && (path == "*" || segment.ident == path))
    })
}

fn generate(attr: TokenStream, item: TokenStream) -> TokenStream {
    let config = AttributeArgs::parse_terminated.parse2(attr).and_then(|args| parse_config(args)).unwrap();

    let input = syn::parse2::<syn::ItemFn>(item).unwrap();

    let ret = &input.sig.output;
    let name = &input.sig.ident;
    let body = &input.block;
    let attrs = &input.attrs;
    let vis = &input.vis;

    let crate_name = config.crate_name.unwrap_or_else(|| Ident::new("asyncs", Span::call_site()));
    let macro_name = format!("#[{crate_name}:test]");

    if input.sig.asyncness.is_none() {
        let err =
            syn::Error::new_spanned(input, format!("only asynchronous function can be tagged with {}", macro_name));
        return TokenStream::from(err.into_compile_error());
    }

    if let Some(attr) = attrs.clone().into_iter().find(is_test_attribute) {
        let msg = "second test attribute is supplied, consider removing or changing the order of your test attributes";
        return TokenStream::from(syn::Error::new_spanned(attr, msg).into_compile_error());
    };

    let prefer_env_parallelism = config.parallelism.is_none();
    let parallelism = config.parallelism.unwrap_or(2);
    quote! {
        #(#attrs)*
        #[::core::prelude::v1::test]
        #vis fn #name() #ret {
            let parallelism = match (#prefer_env_parallelism, #parallelism) {
                (true, parallelism) => match ::std::env::var("ASYNCS_TEST_PARALLELISM") {
                    ::std::result::Result::Err(_) => parallelism,
                    ::std::result::Result::Ok(val) => match val.parse::<usize>() {
                        ::std::result::Result::Err(_) => parallelism,
                        ::std::result::Result::Ok(n) => n,
                    }
                }
                (false, parallelism) => parallelism,
            };
            #crate_name::__executor::Blocking::new(parallelism).block_on(async move #body)
        }
    }
}

/// Converts async function to test against a sample runtime.
///
/// ## Options
/// * `parallelism`: non negative integer to specify parallelism for executor. Defaults to
///   environment variable `ASYNCS_TEST_PARALLELISM` and `2` in fallback. `0` means available
///   cores.
///
/// ## Examples
/// ```ignore
/// use std::future::pending;
///
/// #[asyncs::test]
/// async fn pending_default() {
///     let v = select! {
///         default => 5,
///         i = pending() => i,
///     };
///     assert_eq!(v, 5);
/// }
/// ```
#[proc_macro_attribute]
pub fn test(attr: proc_macro::TokenStream, item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    generate(attr.into(), item.into()).into()
}
