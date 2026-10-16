use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use std::{collections::BTreeSet, str::FromStr};
use syn::{Expr, Ident, LitStr, Token, parse::Parse, parse_macro_input};

include!(concat!(env!("OUT_DIR"), "/codegen.rs"));

/// The translation of `key` into the app's locale, or `key: locale`'s (a `&str`): a `&'static str`.
/// With its placeholders' values, `t!("key", name = value, ...)`, a `String` with `{name}` replaced
/// by `value` (anything `Display`), every placeholder its translations have given.
#[proc_macro]
pub fn t(items: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let Items { key, locale, args } = parse_macro_input!(items as Items);

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

    // Every placeholder given, and nothing else
    let wanted = PLACEHOLDERS
        .get(&key.value())
        .map(|names| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let given = args
        .iter()
        .map(|(name, _)| name.to_string())
        .collect::<BTreeSet<_>>();
    if wanted != given {
        let missing = wanted.difference(&given).collect::<Vec<_>>();
        let extra = given.difference(&wanted).collect::<Vec<_>>();
        return syn::Error::new(
            key.span(),
            format!("Placeholders missing: {missing:?}, not in the translations: {extra:?}"),
        )
        .into_compile_error()
        .into();
    }

    let translated = quote! { crate::i18n::t(const { &#expr }, #locale) };
    if args.is_empty() {
        return translated.into();
    }

    let names = args.iter().map(|(name, _)| name.to_string());
    let values = args.iter().map(|(_, value)| value);
    return quote! {
        crate::i18n::fill(
            #translated,
            &[#((#names, &(#values) as &dyn ::std::fmt::Display)),*],
        )
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
    /// The placeholders' values
    args: Vec<(Ident, Expr)>,
}

impl Parse for Items {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let key = input.parse::<LitStr>()?;
        let mut locale = None;

        if input.parse::<Option<Token![:]>>()?.is_some() {
            locale = Some(input.parse::<Expr>()?);
        }

        let mut args = Vec::new();
        while input.parse::<Option<Token![,]>>()?.is_some() {
            if input.is_empty() {
                break;
            }
            let name = input.parse::<Ident>()?;
            input.parse::<Token![=]>()?;
            args.push((name, input.parse::<Expr>()?));
        }

        return Ok(Self { key, locale, args });
    }
}
