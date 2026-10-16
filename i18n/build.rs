use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env::var_os,
    fs::File,
    io::{BufReader, BufWriter, Write},
    path::PathBuf,
};

use proc_macro2::Span;
use quote::ToTokens;
use syn::LitStr;

fn main() -> std::io::Result<()> {
    let data: HashMap<String, HashMap<String, String>> =
        serde_json::from_reader(BufReader::new(File::open("intl.json")?))?;

    let out_dir = PathBuf::from(var_os("OUT_DIR").unwrap());
    let mut dest = BufWriter::new(File::create(out_dir.join("codegen.rs"))?);

    // The locales to pick from, each by its own name. Every language translated into must have
    // one, or its translations could never be shown
    let locales = data
        .get("language_name")
        .map(|names| names.keys().cloned().collect::<BTreeSet<_>>())
        .unwrap_or_default();
    let language = |locale: &str| locale.split('-').next().unwrap_or_default().to_owned();
    let languages = locales.iter().map(|l| language(l)).collect::<BTreeSet<_>>();
    for (key, translations) in &data {
        for locale in translations.keys() {
            if !languages.contains(&language(locale)) {
                panic!(
                    "\"{key}\" is translated into \"{locale}\", but no \"language_name\" locale \
                     is in its language"
                );
            }
        }
    }

    // `{name}` placeholders: every translation of a key has the same ones
    let mut placeholders = phf_codegen::Map::new();
    for (key, translations) in &data {
        let mut names = None::<(&String, BTreeSet<String>)>;
        for (locale, value) in translations {
            let these = placeholder_names(value);
            match &names {
                Some((first, names)) if *names != these => panic!(
                    "\"{key}\" has placeholders {names:?} in \"{first}\", but {these:?} in \
                     \"{locale}\""
                ),
                Some(_) => {}
                None => names = Some((locale, these)),
            }
        }
        let names = names.map(|(_, names)| names).unwrap_or_default();
        placeholders.entry(
            key.clone(),
            format!("&{:?}", names.iter().collect::<Vec<_>>()),
        );
    }

    let mut outer_map = phf_codegen::Map::new();
    for (key, langs) in data {
        let mut lang_map = BTreeMap::<String, phf_codegen::Map<String>>::new();
        let mut has_english = false;

        for (locale, value) in langs {
            let (lang, country) = match locale.rsplit_once('-') {
                Some((lang, country)) => (lang.to_owned(), Some(country)),
                None => (locale, None),
            };

            has_english |= lang == "en";
            lang_map
                .entry(lang)
                .or_insert_with(|| phf_codegen::Map::new())
                .entry(
                    country.unwrap_or_default().to_owned(),
                    LitStr::new(&value, Span::call_site())
                        .into_token_stream()
                        .to_string(),
                );
        }

        if !has_english {
            panic!("Key \"{key}\" has no English entry");
        }

        let lang_map = lang_map
            .into_iter()
            .map(|(k, v)| (k, v.build().to_string()))
            .collect::<phf_codegen::Map<String>>();

        outer_map.entry(key, format!("r#\"{}\"#", lang_map.build()));
    }

    writeln!(
        &mut dest,
        "static INTL: phf::Map<&'static str, &'static str> = {};",
        outer_map.build()
    )?;
    writeln!(
        &mut dest,
        "static LOCALES: &[&str] = &{:?};",
        locales.iter().collect::<Vec<_>>()
    )?;
    writeln!(
        &mut dest,
        "static PLACEHOLDERS: phf::Map<&'static str, &'static [&'static str]> = {};",
        placeholders.build()
    )?;

    dest.flush()?;
    return Ok(());
}

/// The names of a translation's `{name}` placeholders.
fn placeholder_names(value: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut rest = value;
    while let Some(start) = rest.find('{') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('}') else {
            break;
        };
        let name = &rest[..end];
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            names.insert(name.to_owned());
        }
        rest = &rest[end + 1..];
    }
    names
}
