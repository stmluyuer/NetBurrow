//! Shared desktop language, also used by background status and error messages.
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en")]
    En,
}

static LANGUAGE: AtomicU8 = AtomicU8::new(0);

pub fn language() -> Language {
    if LANGUAGE.load(Ordering::Relaxed) == 1 { Language::En } else { Language::ZhCn }
}

pub fn set_language(language: Language) {
    LANGUAGE.store(u8::from(language == Language::En), Ordering::Relaxed);
}

/// Keep translations together so both languages are checked at compile time.
#[macro_export]
macro_rules! text {
    ($zh:literal, $en:literal $(,)?) => {
        match $crate::i18n::language() {
            $crate::i18n::Language::ZhCn => $zh,
            $crate::i18n::Language::En => $en,
        }
    };
}

#[macro_export]
macro_rules! text_format {
    ($zh:literal, $en:literal $(, $args:expr)* $(,)?) => {
        match $crate::i18n::language() {
            $crate::i18n::Language::ZhCn => format!($zh $(, $args)*),
            $crate::i18n::Language::En => format!($en $(, $args)*),
        }
    };
}
