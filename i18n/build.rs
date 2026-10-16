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

    // Every locale is picked by its own name
    let locales = data
        .values()
        .flat_map(HashMap::keys)
        .cloned()
        .collect::<BTreeSet<_>>();
    for locale in &locales {
        if !data
            .get("language_name")
            .is_some_and(|names| names.contains_key(locale))
        {
            panic!("Locale \"{locale}\" has no \"language_name\" entry");
        }
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

    dest.flush()?;
    return Ok(());
}
