//! Interface languages: every user-facing text exists in Russian and English.

/// Interface language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Ru,
    En,
}

impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Lang::Ru => "ru",
            Lang::En => "en",
        }
    }

    pub fn from_code(code: &str) -> Option<Lang> {
        match code {
            "ru" => Some(Lang::Ru),
            "en" => Some(Lang::En),
            _ => None,
        }
    }

    /// The language for a system locale identifier such as `ru-RU` or `en_GB`.
    pub fn for_locale(locale: &str) -> Lang {
        if locale.to_lowercase().starts_with("ru") {
            Lang::Ru
        } else {
            Lang::En
        }
    }

    /// Pick the text for this language.
    pub fn pick<'a>(self, ru: &'a str, en: &'a str) -> &'a str {
        match self {
            Lang::Ru => ru,
            Lang::En => en,
        }
    }
}

/// A text in both languages.
#[derive(Debug, Clone, Copy)]
pub struct L {
    pub ru: &'static str,
    pub en: &'static str,
}

impl L {
    pub const fn new(ru: &'static str, en: &'static str) -> L {
        L { ru, en }
    }

    pub fn get(self, lang: Lang) -> &'static str {
        lang.pick(self.ru, self.en)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_locales() {
        for l in [Lang::Ru, Lang::En] {
            assert_eq!(Lang::from_code(l.code()), Some(l));
        }
        assert_eq!(Lang::from_code("de"), None);
        assert_eq!(Lang::for_locale("ru-RU"), Lang::Ru);
        assert_eq!(Lang::for_locale("en_GB"), Lang::En);
        assert_eq!(Lang::for_locale("de-DE"), Lang::En);
    }
}
