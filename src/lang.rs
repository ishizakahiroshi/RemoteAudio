//! String lookup for the **host** side of the app (the tray).
//!
//! Reads the same `web/lang/*.json` packs the guest UI uses, so a translation is
//! written once. What is *not* shared is which language wins: the guest picks
//! one in its own browser (`localStorage`), the host follows the Windows UI
//! language. Different machines, different owners — wiring them together would
//! mean one guest's choice re-labelling somebody else's notification area.

use serde_json::Value;

use crate::assets::WebAssets;

/// Pack used when the preferred one is missing a key, or missing entirely.
pub const FALLBACK: &str = "en";

/// Every key the tray can render.
///
/// Exists purely so `cargo test` can prove each language pack carries all of
/// them — a missing key would otherwise show up as a raw identifier in the
/// notification area, the one place nobody looks until a user reports it. Add a
/// key here whenever [`crate::tray`] learns a new string.
#[cfg(test)]
const TRAY_KEYS: &[&str] = &[
    "tray.state.running",
    "tray.state.restarting",
    "tray.state.stopped",
    "tray.state.failed",
    "tray.state.autoRestartOff",
    "tray.tooltip",
    "tray.menu.open",
    "tray.menu.copyShare",
    "tray.menu.createShortcut",
    "tray.menu.restart",
    "tray.menu.stop",
    "tray.menu.start",
    "tray.menu.quit",
    "tray.share.noLan",
    "tray.share.noToken",
    "tray.share.noNic",
    "tray.welcome.title",
    "tray.welcome.body",
    "tray.welcome.setupButton",
    "tray.welcome.setupNote",
    "tray.welcome.setupNotePackaged",
    "tray.welcome.openButton",
    "tray.welcome.openNote",
    "tray.welcome.copyNote",
    "tray.welcome.shortcutNote",
    "tray.welcome.dontShowAgain",
    "tray.alreadyRunning.title",
    "tray.alreadyRunning.body",
    "tray.callout.here",
    "tray.setup.doneTitle",
    "tray.setup.doneBody",
    "tray.setup.noFirewallBody",
    "tray.setup.noShareBody",
    "tray.setup.failedBody",
    "tray.setup.packagedBody",
    "tray.setup.packagedNoShareBody",
    "tray.shortcut.title",
    "tray.shortcut.confirmBody",
    "tray.shortcut.createdBody",
    "tray.shortcut.failedBody",
    "tray.shortcut.noShareBody",
    "tray.share.title",
    "tray.share.copiedBody",
    "tray.share.copyFailedBody",
    "tray.share.noShareBody",
];

/// A resolved language pack plus its English fallback.
pub struct Strings {
    primary: Value,
    fallback: Value,
}

impl Strings {
    /// `preference` is `auto` (follow Windows), or a pack name such as `ja`.
    pub fn load(preference: &str) -> Self {
        Self {
            primary: pack(&resolve(preference)).unwrap_or(Value::Null),
            fallback: pack(FALLBACK).unwrap_or(Value::Null),
        }
    }

    pub fn get(&self, key: &str) -> String {
        lookup(&self.primary, key)
            .or_else(|| lookup(&self.fallback, key))
            // Unreachable while `every_pack_has_every_tray_key` passes. Showing
            // the key beats showing nothing: an empty menu item is invisible,
            // a stray `tray.menu.quit` is a bug report.
            .unwrap_or_else(|| key.to_string())
    }

    /// `get` with `{name}` placeholders substituted, matching the Web UI's
    /// convention so one string can serve both sides.
    pub fn format(&self, key: &str, args: &[(&str, &str)]) -> String {
        let mut text = self.get(key);
        for (name, value) in args {
            text = text.replace(&format!("{{{name}}}"), value);
        }
        text
    }
}

fn lookup(pack: &Value, key: &str) -> Option<String> {
    pack.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

fn pack(code: &str) -> Option<Value> {
    let file = WebAssets::get(&format!("lang/{code}.json"))?;
    serde_json::from_slice(&file.data).ok()
}

/// Turn a preference into a pack name that actually exists.
fn resolve(preference: &str) -> String {
    let want = preference.trim().to_ascii_lowercase();
    let want = if want.is_empty() || want == "auto" {
        detect_ui_language()
    } else {
        want
    };
    if pack(&want).is_some() {
        want
    } else {
        FALLBACK.to_string()
    }
}

/// Windows' UI language, narrowed to a pack we ship.
///
/// Only the primary language id matters here: `ja-JP` and any other Japanese
/// sub-locale should all land on the same pack.
#[cfg(windows)]
fn detect_ui_language() -> String {
    use windows::Win32::Globalization::GetUserDefaultUILanguage;

    const LANG_JAPANESE: u16 = 0x11;
    let primary = unsafe { GetUserDefaultUILanguage() } & 0x3ff;
    if primary == LANG_JAPANESE {
        "ja".to_string()
    } else {
        FALLBACK.to_string()
    }
}

#[cfg(not(windows))]
fn detect_ui_language() -> String {
    FALLBACK.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pack_has_every_tray_key() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/lang");
        let mut packs = 0;

        for entry in std::fs::read_dir(&dir).expect("web/lang is readable") {
            let path = entry.expect("directory entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read language pack");
            let pack: Value = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()));
            packs += 1;

            for key in TRAY_KEYS {
                assert!(
                    lookup(&pack, key).is_some(),
                    "{} is missing {key}",
                    path.display()
                );
            }
        }

        assert!(
            packs >= 2,
            "expected at least the ja and en packs, found {packs}"
        );
    }

    #[test]
    fn an_unknown_preference_falls_back_to_english() {
        assert_eq!(resolve("kl"), FALLBACK);
        assert_eq!(
            Strings::load("kl").get("tray.menu.quit"),
            Strings::load("en").get("tray.menu.quit")
        );
    }

    #[test]
    fn a_named_pack_beats_the_os_language() {
        assert_eq!(resolve("ja"), "ja");
        // Whitespace and case come from a hand-edited config.toml, not from us.
        assert_eq!(resolve("  EN  "), "en");
        // Only `auto` (or an empty value) is allowed to consult Windows.
        assert_eq!(resolve(""), detect_ui_language());
        assert_eq!(resolve("auto"), detect_ui_language());
    }

    #[test]
    fn a_named_pack_really_changes_the_words() {
        assert_ne!(
            Strings::load("ja").get("tray.menu.quit"),
            Strings::load("en").get("tray.menu.quit")
        );
    }

    #[test]
    fn placeholders_are_substituted_by_name() {
        let strings = Strings::load("en");
        let tip = strings.format(
            "tray.tooltip",
            &[("state", "Running"), ("address", "127.0.0.1:17650")],
        );
        assert!(tip.contains("Running"), "{tip}");
        assert!(tip.contains("127.0.0.1:17650"), "{tip}");
        assert!(
            !tip.contains('{'),
            "unsubstituted placeholder left in {tip}"
        );
    }
}
