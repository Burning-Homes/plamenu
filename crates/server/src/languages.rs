//! The posting-language inventory: every locale Mastodon 4.6 accepts as a
//! posting language, with its English and native names. Generated to match
//! that inventory — regenerate rather than hand-editing if the parity target
//! moves.
//!
//! The slice is pre-sorted for dropdown display the way Mastodon sorts its
//! language picker: by ASCII-folded, lowercased native name.

/// One selectable posting language.
pub struct Language {
    /// ISO 639-1/639-3 code, possibly with a regional/script subtag
    /// (`zh-TW`, `mn-Mong`) — exactly Mastodon's key set.
    pub code: &'static str,
    pub english: &'static str,
    pub native: &'static str,
}

impl Language {
    /// The dropdown/checklist label: the native name, with the English name
    /// alongside when it differs ("Deutsch (German)").
    #[must_use]
    pub fn label(&self) -> String {
        if self.native == self.english {
            self.native.to_owned()
        } else {
            format!("{} ({})", self.native, self.english)
        }
    }
}

/// All supported posting languages, in display order.
pub static LANGUAGES: &[Language] = &[
    Language {
        code: "sma",
        english: "Southern Sami",
        native: "Åarjelsaemien Gïele",
    },
    Language {
        code: "om",
        english: "Oromo",
        native: "Afaan Oromoo",
    },
    Language {
        code: "aa",
        english: "Afar",
        native: "Afaraf",
    },
    Language {
        code: "af",
        english: "Afrikaans",
        native: "Afrikaans",
    },
    Language {
        code: "ak",
        english: "Akan",
        native: "Akan",
    },
    Language {
        code: "an",
        english: "Aragonese",
        native: "aragonés",
    },
    Language {
        code: "ast",
        english: "Asturian",
        native: "Asturianu",
    },
    Language {
        code: "ig",
        english: "Igbo",
        native: "Asụsụ Igbo",
    },
    Language {
        code: "ae",
        english: "Avestan",
        native: "avesta",
    },
    Language {
        code: "ay",
        english: "Aymara",
        native: "aymar aru",
    },
    Language {
        code: "az",
        english: "Azerbaijani",
        native: "azərbaycan dili",
    },
    Language {
        code: "id",
        english: "Indonesian",
        native: "Bahasa Indonesia",
    },
    Language {
        code: "ms",
        english: "Malay",
        native: "Bahasa Melayu",
    },
    Language {
        code: "bm",
        english: "Bambara",
        native: "bamanankan",
    },
    Language {
        code: "jv",
        english: "Javanese",
        native: "basa Jawa",
    },
    Language {
        code: "su",
        english: "Sundanese",
        native: "Basa Sunda",
    },
    Language {
        code: "bi",
        english: "Bislama",
        native: "Bislama",
    },
    Language {
        code: "bs",
        english: "Bosnian",
        native: "bosanski jezik",
    },
    Language {
        code: "br",
        english: "Breton",
        native: "brezhoneg",
    },
    Language {
        code: "ca",
        english: "Catalan",
        native: "Català",
    },
    Language {
        code: "cs",
        english: "Czech",
        native: "čeština",
    },
    Language {
        code: "ch",
        english: "Chamorro",
        native: "Chamoru",
    },
    Language {
        code: "ny",
        english: "Chichewa",
        native: "chiCheŵa",
    },
    Language {
        code: "sn",
        english: "Shona",
        native: "chiShona",
    },
    Language {
        code: "co",
        english: "Corsican",
        native: "corsu",
    },
    Language {
        code: "cnr",
        english: "Montenegrin",
        native: "crnogorski",
    },
    Language {
        code: "cy",
        english: "Welsh",
        native: "Cymraeg",
    },
    Language {
        code: "da",
        english: "Danish",
        native: "dansk",
    },
    Language {
        code: "se",
        english: "Northern Sami",
        native: "Davvisámegiella",
    },
    Language {
        code: "de",
        english: "German",
        native: "Deutsch",
    },
    Language {
        code: "nv",
        english: "Navajo",
        native: "Diné bizaad",
    },
    Language {
        code: "et",
        english: "Estonian",
        native: "eesti",
    },
    Language {
        code: "na",
        english: "Nauru",
        native: "Ekakairũ Naoero",
    },
    Language {
        code: "en",
        english: "English",
        native: "English",
    },
    Language {
        code: "es",
        english: "Spanish",
        native: "Español",
    },
    Language {
        code: "eo",
        english: "Esperanto",
        native: "Esperanto",
    },
    Language {
        code: "eu",
        english: "Basque",
        native: "euskara",
    },
    Language {
        code: "ee",
        english: "Ewe",
        native: "Eʋegbe",
    },
    Language {
        code: "to",
        english: "Tonga",
        native: "faka Tonga",
    },
    Language {
        code: "mg",
        english: "Malagasy",
        native: "fiteny malagasy",
    },
    Language {
        code: "fr",
        english: "French",
        native: "Français",
    },
    Language {
        code: "fy",
        english: "Western Frisian",
        native: "Frysk",
    },
    Language {
        code: "ff",
        english: "Fula",
        native: "Fulfulde",
    },
    Language {
        code: "fo",
        english: "Faroese",
        native: "føroyskt",
    },
    Language {
        code: "ga",
        english: "Irish",
        native: "Gaeilge",
    },
    Language {
        code: "gv",
        english: "Manx",
        native: "Gaelg",
    },
    Language {
        code: "gd",
        english: "Scottish Gaelic",
        native: "Gàidhlig",
    },
    Language {
        code: "gl",
        english: "Galician",
        native: "galego",
    },
    Language {
        code: "ki",
        english: "Kikuyu",
        native: "Gĩkũyũ",
    },
    Language {
        code: "ho",
        english: "Hiri Motu",
        native: "Hiri Motu",
    },
    Language {
        code: "hr",
        english: "Croatian",
        native: "Hrvatski",
    },
    Language {
        code: "io",
        english: "Ido",
        native: "Ido",
    },
    Language {
        code: "rw",
        english: "Kinyarwanda",
        native: "Ikinyarwanda",
    },
    Language {
        code: "rn",
        english: "Kirundi",
        native: "Ikirundi",
    },
    Language {
        code: "ia",
        english: "Interlingua",
        native: "Interlingua",
    },
    Language {
        code: "ie",
        english: "Interlingue",
        native: "Interlingue",
    },
    Language {
        code: "ik",
        english: "Inupiaq",
        native: "Iñupiaq",
    },
    Language {
        code: "nd",
        english: "Northern Ndebele",
        native: "isiNdebele",
    },
    Language {
        code: "nr",
        english: "Southern Ndebele",
        native: "isiNdebele",
    },
    Language {
        code: "xh",
        english: "Xhosa",
        native: "isiXhosa",
    },
    Language {
        code: "zu",
        english: "Zulu",
        native: "isiZulu",
    },
    Language {
        code: "is",
        english: "Icelandic",
        native: "Íslenska",
    },
    Language {
        code: "it",
        english: "Italian",
        native: "Italiano",
    },
    Language {
        code: "smj",
        english: "Lule Sami",
        native: "Julevsámegiella",
    },
    Language {
        code: "mh",
        english: "Marshallese",
        native: "Kajin M̧ajeļ",
    },
    Language {
        code: "kl",
        english: "Kalaallisut",
        native: "kalaallisut",
    },
    Language {
        code: "moh",
        english: "Mohawk",
        native: "Kanienʼkéha",
    },
    Language {
        code: "kr",
        english: "Kanuri",
        native: "Kanuri",
    },
    Language {
        code: "csb",
        english: "Kashubian",
        native: "Kaszëbsczi",
    },
    Language {
        code: "kw",
        english: "Cornish",
        native: "Kernewek",
    },
    Language {
        code: "kg",
        english: "Kongo",
        native: "Kikongo",
    },
    Language {
        code: "sw",
        english: "Swahili",
        native: "Kiswahili",
    },
    Language {
        code: "ht",
        english: "Haitian",
        native: "Kreyòl ayisyen",
    },
    Language {
        code: "kj",
        english: "Kwanyama",
        native: "Kuanyama",
    },
    Language {
        code: "ku",
        english: "Kurmanji (Kurdish)",
        native: "Kurmancî",
    },
    Language {
        code: "jbo",
        english: "Lojban",
        native: "la .lojban.",
    },
    Language {
        code: "ldn",
        english: "Láadan",
        native: "Láadan",
    },
    Language {
        code: "la",
        english: "Latin",
        native: "latine",
    },
    Language {
        code: "lv",
        english: "Latvian",
        native: "Latviski",
    },
    Language {
        code: "lb",
        english: "Luxembourgish",
        native: "Lëtzebuergesch",
    },
    Language {
        code: "lt",
        english: "Lithuanian",
        native: "lietuvių kalba",
    },
    Language {
        code: "li",
        english: "Limburgish",
        native: "Limburgs",
    },
    Language {
        code: "ln",
        english: "Lingala",
        native: "Lingála",
    },
    Language {
        code: "lfn",
        english: "Lingua Franca Nova",
        native: "lingua franca nova",
    },
    Language {
        code: "lg",
        english: "Ganda",
        native: "Luganda",
    },
    Language {
        code: "hu",
        english: "Hungarian",
        native: "magyar",
    },
    Language {
        code: "mt",
        english: "Maltese",
        native: "Malti",
    },
    Language {
        code: "nl",
        english: "Dutch",
        native: "Nederlands",
    },
    Language {
        code: "no",
        english: "Norwegian",
        native: "Norsk",
    },
    Language {
        code: "nb",
        english: "Norwegian Bokmål",
        native: "Norsk bokmål",
    },
    Language {
        code: "nn",
        english: "Norwegian Nynorsk",
        native: "Norsk Nynorsk",
    },
    Language {
        code: "oc",
        english: "Occitan",
        native: "occitan",
    },
    Language {
        code: "hz",
        english: "Herero",
        native: "Otjiherero",
    },
    Language {
        code: "ng",
        english: "Ndonga",
        native: "Owambo",
    },
    Language {
        code: "pdc",
        english: "Pennsylvania Dutch",
        native: "Pennsilfaani-Deitsch",
    },
    Language {
        code: "nds",
        english: "Low German",
        native: "Plattdüütsch",
    },
    Language {
        code: "pl",
        english: "Polish",
        native: "Polski",
    },
    Language {
        code: "pt",
        english: "Portuguese",
        native: "Português",
    },
    Language {
        code: "ty",
        english: "Tahitian",
        native: "Reo Tahiti",
    },
    Language {
        code: "ro",
        english: "Romanian",
        native: "Română",
    },
    Language {
        code: "rm",
        english: "Romansh",
        native: "rumantsch grischun",
    },
    Language {
        code: "qu",
        english: "Quechua",
        native: "Runa Simi",
    },
    Language {
        code: "sc",
        english: "Sardinian",
        native: "sardu",
    },
    Language {
        code: "za",
        english: "Zhuang",
        native: "Saɯ cueŋƅ",
    },
    Language {
        code: "gsw",
        english: "Swiss German",
        native: "Schwiizertütsch",
    },
    Language {
        code: "sco",
        english: "Scots",
        native: "Scots",
    },
    Language {
        code: "st",
        english: "Southern Sotho",
        native: "Sesotho",
    },
    Language {
        code: "tn",
        english: "Tswana",
        native: "Setswana",
    },
    Language {
        code: "sq",
        english: "Albanian",
        native: "Shqip",
    },
    Language {
        code: "ss",
        english: "Swati",
        native: "SiSwati",
    },
    Language {
        code: "sk",
        english: "Slovak",
        native: "slovenčina",
    },
    Language {
        code: "sl",
        english: "Slovenian",
        native: "slovenščina",
    },
    Language {
        code: "szl",
        english: "Silesian",
        native: "ślůnsko godka",
    },
    Language {
        code: "so",
        english: "Somali",
        native: "Soomaaliga",
    },
    Language {
        code: "fi",
        english: "Finnish",
        native: "suomi",
    },
    Language {
        code: "sv",
        english: "Swedish",
        native: "Svenska",
    },
    Language {
        code: "tl",
        english: "Tagalog",
        native: "Tagalog",
    },
    Language {
        code: "kab",
        english: "Kabyle",
        native: "Taqbaylit",
    },
    Language {
        code: "mi",
        english: "Māori",
        native: "te reo Māori",
    },
    Language {
        code: "vi",
        english: "Vietnamese",
        native: "Tiếng Việt",
    },
    Language {
        code: "tok",
        english: "Toki Pona",
        native: "toki pona",
    },
    Language {
        code: "lu",
        english: "Luba-Katanga",
        native: "Tshiluba",
    },
    Language {
        code: "ve",
        english: "Venda",
        native: "Tshivenḓa",
    },
    Language {
        code: "tr",
        english: "Turkish",
        native: "Türkçe",
    },
    Language {
        code: "tk",
        english: "Turkmen",
        native: "Türkmen",
    },
    Language {
        code: "tw",
        english: "Twi",
        native: "Twi",
    },
    Language {
        code: "fj",
        english: "Fijian",
        native: "Vakaviti",
    },
    Language {
        code: "vo",
        english: "Volapük",
        native: "Volapük",
    },
    Language {
        code: "wa",
        english: "Walloon",
        native: "walon",
    },
    Language {
        code: "wo",
        english: "Wolof",
        native: "Wollof",
    },
    Language {
        code: "ts",
        english: "Tsonga",
        native: "Xitsonga",
    },
    Language {
        code: "sg",
        english: "Sango",
        native: "yângâ tî sängö",
    },
    Language {
        code: "yo",
        english: "Yoruba",
        native: "Yorùbá",
    },
    Language {
        code: "el",
        english: "Greek",
        native: "Ελληνικά",
    },
    Language {
        code: "av",
        english: "Avaric",
        native: "авар мацӀ",
    },
    Language {
        code: "ab",
        english: "Abkhaz",
        native: "аҧсуа бызшәа",
    },
    Language {
        code: "ba",
        english: "Bashkir",
        native: "башҡорт теле",
    },
    Language {
        code: "be",
        english: "Belarusian",
        native: "беларуская мова",
    },
    Language {
        code: "bg",
        english: "Bulgarian",
        native: "български език",
    },
    Language {
        code: "os",
        english: "Ossetian",
        native: "ирон æвзаг",
    },
    Language {
        code: "kv",
        english: "Komi",
        native: "коми кыв",
    },
    Language {
        code: "ky",
        english: "Kyrgyz",
        native: "Кыргызча",
    },
    Language {
        code: "mk",
        english: "Macedonian",
        native: "македонски јазик",
    },
    Language {
        code: "mn",
        english: "Mongolian",
        native: "Монгол хэл",
    },
    Language {
        code: "ce",
        english: "Chechen",
        native: "нохчийн мотт",
    },
    Language {
        code: "ru",
        english: "Russian",
        native: "Русский",
    },
    Language {
        code: "sr",
        english: "Serbian",
        native: "српски језик",
    },
    Language {
        code: "tt",
        english: "Tatar",
        native: "татар теле",
    },
    Language {
        code: "tg",
        english: "Tajik",
        native: "тоҷикӣ",
    },
    Language {
        code: "uz",
        english: "Uzbek",
        native: "Ўзбек",
    },
    Language {
        code: "uk",
        english: "Ukrainian",
        native: "Українська",
    },
    Language {
        code: "xal",
        english: "Kalmyk",
        native: "Хальмг келн",
    },
    Language {
        code: "cv",
        english: "Chuvash",
        native: "чӑваш чӗлхи",
    },
    Language {
        code: "cu",
        english: "Old Church Slavonic",
        native: "ѩзыкъ словѣньскъ",
    },
    Language {
        code: "kk",
        english: "Kazakh",
        native: "қазақ тілі",
    },
    Language {
        code: "hy",
        english: "Armenian",
        native: "Հայերեն",
    },
    Language {
        code: "yi",
        english: "Yiddish",
        native: "ייִדיש",
    },
    Language {
        code: "he",
        english: "Hebrew",
        native: "עברית",
    },
    Language {
        code: "ur",
        english: "Urdu",
        native: "اردو",
    },
    Language {
        code: "ar",
        english: "Arabic",
        native: "اللغة العربية",
    },
    Language {
        code: "zba",
        english: "Balaibalan",
        native: "باليبلن",
    },
    Language {
        code: "ms-Arab",
        english: "Jawi Malay",
        native: "بهاس ملايو",
    },
    Language {
        code: "ckb",
        english: "Sorani (Kurdish)",
        native: "سۆرانی",
    },
    Language {
        code: "fa",
        english: "Persian",
        native: "فارسی",
    },
    Language {
        code: "ota",
        english: "Ottoman Turkish",
        native: "لسان عثمانی",
    },
    Language {
        code: "ha",
        english: "Hausa",
        native: "هَوُسَ",
    },
    Language {
        code: "ug",
        english: "Uyghur",
        native: "ئۇيغۇرچە‎",
    },
    Language {
        code: "ps",
        english: "Pashto",
        native: "پښتو",
    },
    Language {
        code: "dv",
        english: "Divehi",
        native: "ދިވެހި",
    },
    Language {
        code: "ks",
        english: "Kashmiri",
        native: "कश्मीरी",
    },
    Language {
        code: "ne",
        english: "Nepali",
        native: "नेपाली",
    },
    Language {
        code: "pi",
        english: "Pāli",
        native: "पाऴि",
    },
    Language {
        code: "bh",
        english: "Bihari",
        native: "भोजपुरी",
    },
    Language {
        code: "mr",
        english: "Marathi",
        native: "मराठी",
    },
    Language {
        code: "sa",
        english: "Sanskrit",
        native: "संस्कृतम्",
    },
    Language {
        code: "sd",
        english: "Sindhi",
        native: "सिन्धी",
    },
    Language {
        code: "hi",
        english: "Hindi",
        native: "हिन्दी",
    },
    Language {
        code: "as",
        english: "Assamese",
        native: "অসমীয়া",
    },
    Language {
        code: "bn",
        english: "Bengali",
        native: "বাংলা",
    },
    Language {
        code: "pa",
        english: "Punjabi",
        native: "ਪੰਜਾਬੀ",
    },
    Language {
        code: "gu",
        english: "Gujarati",
        native: "ગુજરાતી",
    },
    Language {
        code: "or",
        english: "Oriya",
        native: "ଓଡ଼ିଆ",
    },
    Language {
        code: "ta",
        english: "Tamil",
        native: "தமிழ்",
    },
    Language {
        code: "te",
        english: "Telugu",
        native: "తెలుగు",
    },
    Language {
        code: "kn",
        english: "Kannada",
        native: "ಕನ್ನಡ",
    },
    Language {
        code: "ml",
        english: "Malayalam",
        native: "മലയാളം",
    },
    Language {
        code: "si",
        english: "Sinhala",
        native: "සිංහල",
    },
    Language {
        code: "th",
        english: "Thai",
        native: "ไทย",
    },
    Language {
        code: "lo",
        english: "Lao",
        native: "ລາວ",
    },
    Language {
        code: "bo",
        english: "Tibetan",
        native: "བོད་ཡིག",
    },
    Language {
        code: "dz",
        english: "Dzongkha",
        native: "རྫོང་ཁ",
    },
    Language {
        code: "my",
        english: "Burmese",
        native: "ဗမာစာ",
    },
    Language {
        code: "lzz",
        english: "Lazuri",
        native: "ლაზური ნენა",
    },
    Language {
        code: "xmf",
        english: "Mingrelian",
        native: "მარგალური ნინა",
    },
    Language {
        code: "ka",
        english: "Georgian",
        native: "ქართული",
    },
    Language {
        code: "ko",
        english: "Korean",
        native: "한국어",
    },
    Language {
        code: "ti",
        english: "Tigrinya",
        native: "ትግርኛ",
    },
    Language {
        code: "am",
        english: "Amharic",
        native: "አማርኛ",
    },
    Language {
        code: "iu",
        english: "Inuktitut",
        native: "ᐃᓄᒃᑎᑐᑦ",
    },
    Language {
        code: "oj",
        english: "Ojibwe",
        native: "ᐊᓂᔑᓈᐯᒧᐎᓐ",
    },
    Language {
        code: "cr",
        english: "Cree",
        native: "ᓀᐦᐃᔭᐍᐏᐣ",
    },
    Language {
        code: "km",
        english: "Khmer",
        native: "ខេមរភាសា",
    },
    Language {
        code: "mn-Mong",
        english: "Traditional Mongolian",
        native: "ᠮᠣᠩᠭᠣᠯ ᠬᠡᠯᠡ",
    },
    Language {
        code: "zgh",
        english: "Standard Moroccan Tamazight",
        native: "ⵜⴰⵎⴰⵣⵉⵖⵜ",
    },
    Language {
        code: "zh",
        english: "Chinese",
        native: "中文",
    },
    Language {
        code: "zh-YUE",
        english: "Cantonese",
        native: "廣東話",
    },
    Language {
        code: "ja",
        english: "Japanese",
        native: "日本語",
    },
    Language {
        code: "zh-CN",
        english: "Chinese (China)",
        native: "简体中文",
    },
    Language {
        code: "zh-TW",
        english: "Chinese (Taiwan)",
        native: "繁體中文（臺灣）",
    },
    Language {
        code: "zh-HK",
        english: "Chinese (Hong Kong)",
        native: "繁體中文（香港）",
    },
    Language {
        code: "nan-TW",
        english: "Hokkien (Taiwan)",
        native: "臺語 (Hô-ló話)",
    },
    Language {
        code: "ii",
        english: "Nuosu",
        native: "ꆈꌠ꒿ Nuosuhxop",
    },
    Language {
        code: "vai",
        english: "Vai",
        native: "ꕙꔤ",
    },
    Language {
        code: "chr",
        english: "Cherokee",
        native: "ᏣᎳᎩ ᎦᏬᏂᎯᏍᏗ",
    },
];

/// Looks a language up by its exact code.
#[must_use]
pub fn find(code: &str) -> Option<&'static Language> {
    LANGUAGES.iter().find(|language| language.code == code)
}

/// The subset of the inventory a user enabled for posting, in display order.
/// `None` (never configured) or a set matching nothing yields the whole
/// inventory — a composer offering zero languages would be unusable.
#[must_use]
pub fn enabled(codes: Option<&[String]>) -> Vec<&'static Language> {
    if let Some(codes) = codes {
        let subset: Vec<&'static Language> = LANGUAGES
            .iter()
            .filter(|language| codes.iter().any(|code| code == language.code))
            .collect();
        if !subset.is_empty() {
            return subset;
        }
    }
    LANGUAGES.iter().collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn codes_are_unique_and_inventory_is_complete() {
        let codes: HashSet<&str> = LANGUAGES.iter().map(|l| l.code).collect();
        assert_eq!(codes.len(), LANGUAGES.len());
        // Mastodon 4.6 ships 184 ISO 639-1 + 4 regional + 25 ISO 639-3 + 1.
        assert_eq!(LANGUAGES.len(), 214);
        assert!(codes.contains("en") && codes.contains("zh-TW") && codes.contains("tok"));
    }

    #[test]
    fn find_matches_exact_codes_only() {
        assert_eq!(find("de").unwrap().english, "German");
        assert_eq!(find("de").unwrap().label(), "Deutsch (German)");
        assert_eq!(find("en").unwrap().label(), "English");
        assert!(find("xx").is_none());
        assert!(find("DE").is_none());
    }
}
