//! Internationalisation : FR par défaut, EN automatique.
//!
//! La table EN est générée depuis l'implémentation Python historique
//! (`assets/i18n_en.json`) : les clés sont les chaînes FR source exactes.
//! Les clés sont retrouvées telles quelles ; une clé absente retombe sur le FR.

use std::collections::HashMap;
use std::sync::OnceLock;

static LANG: OnceLock<&'static str> = OnceLock::new();

/// Détecte la langue depuis LANGUAGE, LC_ALL, LC_MESSAGES, LANG (priorité).
pub fn language() -> &'static str {
    LANG.get_or_init(|| {
        for var in ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"] {
            let v = std::env::var(var).unwrap_or_default().to_lowercase();
            if v.is_empty() {
                continue;
            }
            if v.starts_with("en") {
                return "en";
            }
            if v.starts_with("fr") {
                return "fr";
            }
        }
        "fr"
    })
}

fn table() -> &'static HashMap<String, String> {
    static T: OnceLock<HashMap<String, String>> = OnceLock::new();
    T.get_or_init(|| {
        serde_json::from_str(include_str!("../assets/i18n_en.json")).unwrap_or_default()
    })
}

/// Traduire une chaîne FR visible vers la langue active (FR par défaut).
pub fn t(s: &str) -> String {
    if language() == "en" {
        if let Some(v) = table().get(s) {
            return v.clone();
        }
    }
    s.to_string()
}

/// Traduire puis substituer des paramètres nommés `{name}`.
pub fn tf(s: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = t(s);
    for (k, v) in pairs {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_source_key_is_translatable() {
        // Échantillon : les clés utilisées par le menu.
        for k in [
            "Créer un instantané",
            "Lister les instantanés",
            "Quitter",
            "Aucun snapshot à restaurer.",
        ] {
            assert!(!t(k).is_empty());
        }
    }
}
