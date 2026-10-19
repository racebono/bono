use phf::phf_map;
use std::{
    mem::{MaybeUninit, transmute},
    ops::Deref,
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Copy)]
#[repr(C, align(8))]
pub struct Locale {
    len: u8,
    data: [MaybeUninit<u8>; 7],
}

impl Locale {
    pub const fn new(s: &str) -> Option<Self> {
        if s.len() > 7 {
            return None;
        }

        let mut data = unsafe {
            transmute::<MaybeUninit<[u8; 7]>, [MaybeUninit<u8>; 7]>(MaybeUninit::zeroed())
        };

        unsafe { std::ptr::copy_nonoverlapping(s.as_ptr(), data.as_mut_ptr().cast(), s.len()) };
        return Some(Self {
            len: s.len() as u8,
            data,
        });
    }

    #[inline]
    const fn from_bits(bits: u64) -> Self {
        unsafe { transmute::<[u8; 8], Self>(bits.to_ne_bytes()) }
    }

    #[inline]
    const fn to_bits(self) -> u64 {
        u64::from_ne_bytes(unsafe { transmute::<Self, [u8; 8]>(self) })
    }
}

impl Deref for Locale {
    type Target = str;

    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe {
            std::str::from_utf8_unchecked(
                self.data
                    .get_unchecked(..self.len as usize)
                    .assume_init_ref(),
            )
        }
    }
}

static CURRENT_LOCALE: AtomicU64 = AtomicU64::new(Locale::new("en-US").unwrap().to_bits());
static LANGUAGE_FALLBACKS: phf::Map<&'static str, &'static str> = phf_map! {
    "ca" => "es",
};

#[inline]
pub fn locale() -> Locale {
    Locale::from_bits(CURRENT_LOCALE.load(Ordering::Relaxed))
}

#[must_use]
#[inline]
pub fn set_locale(locale: &str) -> bool {
    if let Some(locale) = Locale::new(locale) {
        CURRENT_LOCALE.store(locale.to_bits(), Ordering::Relaxed);
        return true;
    } else {
        return false;
    }
}

#[doc(hidden)]
pub fn t(
    values: &'static phf::Map<&'static str, phf::Map<&'static str, &'static str>>,
    target: &str,
) -> &'static str {
    let (target_lang, target_country) = match target.rsplit_once('-') {
        Some((lang, country)) => (lang, country),
        None => (target, ""),
    };

    return t_impl(values, target_lang, target_country);
}

/// `template` with each `{name}` replaced by its value.
#[doc(hidden)]
pub fn fill(template: &str, values: &[(&str, &dyn std::fmt::Display)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let value = after.find('}').and_then(|end| {
            let name = &after[..end];
            let (_, value) = values.iter().find(|(n, _)| *n == name)?;
            Some((value, end))
        });
        match value {
            Some((value, end)) => {
                use std::fmt::Write;
                let _ = write!(out, "{value}");
                rest = &after[end + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[doc(hidden)]
fn t_impl(
    values: &'static phf::Map<&'static str, phf::Map<&'static str, &'static str>>,
    target_lang: &str,
    target_country: &str,
) -> &'static str {
    let by_lang = t_by_lang(values, target_lang, target_country);
    if target_lang == "en" {
        return unsafe { by_lang.unwrap_unchecked() };
    }

    if let Some(entry) = by_lang {
        return entry;
    }

    return t_impl(
        values,
        LANGUAGE_FALLBACKS.get(target_lang).copied().unwrap_or("en"),
        target_country,
    );
}

#[doc(hidden)]
fn t_by_lang(
    values: &'static phf::Map<&'static str, phf::Map<&'static str, &'static str>>,
    target_lang: &str,
    target_country: &str,
) -> Option<&'static str> {
    let by_country = values.get(target_lang)?;

    if let Some(&entry) = by_country.get(target_country) {
        return Some(entry);
    }

    if !target_country.is_empty() {
        if let Some(&entry) = by_country.get("") {
            return Some(entry);
        }
    }

    return Some(unsafe { by_country.values().next().unwrap_unchecked() });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bono_i18n::{locales, t};

    #[test]
    fn fills_the_placeholders() {
        let values: [(&str, &dyn std::fmt::Display); 2] = [("name", &"Spa"), ("n", &3)];
        assert_eq!(fill("{name}: {n} laps", &values), "Spa: 3 laps");
        // Not a placeholder: left as it is
        assert_eq!(fill("{other} {", &values), "{other} {");
    }

    #[test]
    fn translates_into_every_locale() {
        assert_eq!(locales!(), ["ca-ES", "en-GB", "en-US", "es-ES"]);
        assert_eq!(t!("settings": "es"), "Ajustes");
        assert_eq!(t!("settings": "es-MX"), "Ajustes");
        assert_eq!(t!("settings": "en-US"), "Settings");
        // Not translated yet: English
        assert_eq!(t!("audio": "es"), "Audio");
        assert_eq!(t!("settings": "de"), "Settings");
        assert_eq!(t!("settings": "ca"), "Configuració");
        assert_eq!(t!("settings": "ca-ES"), "Configuració");

        for locale in locales!() {
            let name = t!("language_name": locale);
            assert!(!name.is_empty(), "{locale}");
        }
        assert_eq!(t!("language_name": "es-ES"), "Español (España)");
        assert_eq!(t!("language_name": "ca-ES"), "Català (Espanya)");
    }

    #[test]
    fn keeps_the_locale_whole() {
        for locale in ["es", "en-US", "pt-BR", "zh-Hant"] {
            assert_eq!(&*Locale::new(locale).unwrap(), locale);
            assert_eq!(
                &*Locale::from_bits(Locale::new(locale).unwrap().to_bits()),
                locale
            );
        }
        assert!(Locale::new("too-long").is_none());
    }
}
