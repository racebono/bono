use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use std::str::FromStr;
use syn::{Expr, LitStr, Token, parse::Parse, parse_macro_input};

include!(concat!(env!("OUT_DIR"), "/codegen.rs"));

#[proc_macro]
pub fn t(items: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let Items { key, locale } = parse_macro_input!(items as Items);

    let Some(&entry) = INTL.get(&key.value()) else {
        return syn::Error::new(key.span(), "Invalid i18n key")
            .into_compile_error()
            .into();
    };

    let expr = match syn::parse2::<Expr>(TokenStream::from_str(entry).unwrap()) {
        Ok(expr) => expr,
        Err(e) => return e.into_compile_error().into(),
    };

    let locale = match locale {
        Some(locale) => locale.into_token_stream(),
        None => quote! { &crate::i18n::locale() },
    };

    return quote! {
        crate::i18n::t(const { &#expr }, #locale)
    }
    .into();
}

/// Every locale with a translation, e.g. `["en", "es"]`.
#[proc_macro]
pub fn locales(_: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let locales = LOCALES.iter();
    return quote! { [#(#locales),*] }.into();
}

struct Items {
    key: LitStr,
    /// The locale to translate into (a `&str`): the app's when `None`
    locale: Option<Expr>,
}

impl Parse for Items {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let key = input.parse::<LitStr>()?;
        let mut locale = None;

        if input.parse::<Option<Token![:]>>()?.is_some() {
            locale = Some(input.parse::<Expr>()?);
        }

        return Ok(Self { key, locale });
    }
}
