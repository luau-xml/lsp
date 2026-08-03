//! The project's own spelling of Roblox's names.
//!
//! `[elements] all` and `[properties] all` rename every class, property and
//! event at once, and a rename **retires** the original — so in a project with
//! `all = "camelCase"`, `<TextLabel>` is an error and `<textLabel>` is the tag.
//! The rule that follows is the whole reason this module exists:
//!
//! > the server must never show a name the user could not write in their file.
//!
//! Everything the server *produces* — completion labels, did-you-mean lists,
//! hover text — has to be generated in the project's spelling rather than echoed
//! from the Roblox tables.
//!
//! The transform itself is the compiler's ([`Casing::apply`]), and so is the rule
//! for what happens when two names collide ([`config::preferred`]). What is left
//! here is the direction the compiler does not expose: canonical → written, for
//! every name at once, which is what completion enumerates.
//!
//! **Correctness comes from asking, not from reimplementing.** A candidate
//! spelling is offered only once [`Config::resolve_element`] confirms it maps
//! back to the class it came from, which is the round trip that matters: every
//! name offered is one the compiler then accepts. That also settles the two
//! cases a transform alone gets wrong — a class that lost a collision (its
//! spelling belongs to the other one) and a class that opted back out of the
//! scheme with an identity entry (its spelling is the canonical after all).
//!
//! Asking costs ~90ms across the class list, so it is done **once per config**
//! and only when a scheme is actually set. A project without one — the default,
//! and most of them — pays nothing at all, since with no scheme there are no
//! collisions and the written name is simply the alias or the canonical.

use luaux::config::{self, Casing, Config};
use luaux::roblox;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

/// A Roblox name and how this project writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    /// What the user types, and what completion offers.
    pub written: String,
    /// What Roblox calls it, and what the API tables are keyed by.
    pub canonical: &'static str,
}

/// A project's vocabulary, derived from its `luaux.toml`.
///
/// Built lazily and kept per project: a scheme makes this expensive enough to be
/// worth doing once, and cheap enough afterwards to sit on the keystroke path.
#[derive(Default)]
pub struct Vocabulary {
    elements: OnceLock<Vec<Named>>,
    /// Per class, since inheritance decides which members exist.
    members: Mutex<HashMap<String, Vec<Named>>>,
}

impl Vocabulary {
    /// Every element this project may write, in its own spelling.
    pub fn elements(&self, config: &Config) -> &[Named] {
        self.elements.get_or_init(|| {
            let casing = config.element_casing();

            roblox::creatable_classes()
                .filter_map(|class| {
                    let written = spellings(config.element_name(class), class, casing)
                        .into_iter()
                        // The compiler is the authority on whether this spelling
                        // is the one that reaches this class.
                        .find(|written| {
                            casing == Casing::Pascal
                                || config.resolve_element(written) == Ok(Some(class))
                        })?;

                    Some(Named { written, canonical: class })
                })
                .collect()
        })
    }

    /// Every property and event on `class`, in this project's spelling.
    ///
    /// Collisions are resolved the compiler's way — the non-deprecated name
    /// wins, then byte order — because offering both would show two identical
    /// entries that mean different things.
    pub fn members(&self, config: &Config, class: &str) -> Vec<Named> {
        if let Some(cached) = self.members.lock().ok().and_then(|m| m.get(class).cloned()) {
            return cached;
        }

        let casing = config.property_casing();
        let mut by_written: BTreeMap<String, Vec<&'static str>> = BTreeMap::new();

        for canonical in roblox::properties(class).chain(roblox::events(class)) {
            let alias = config.property_name(class, canonical);

            // An explicit entry stands on its own; only names no entry claimed
            // go through the scheme, which is the compiler's own precedence.
            let written = match alias == canonical {
                false => alias.to_string(),
                true => casing.apply(canonical),
            };

            by_written.entry(written).or_default().push(canonical);
        }

        let named: Vec<Named> = by_written
            .into_iter()
            .filter_map(|(written, mut candidates)| {
                let canonical = config::preferred(&mut candidates)?;

                // Same round trip as for elements. It is what catches an
                // identity entry, whose spelling the scheme would otherwise
                // have taken.
                (casing == Casing::Pascal
                    || config.resolve_property(class, &written).as_deref() == Ok(canonical))
                .then_some(Named { written, canonical })
            })
            .collect();

        if let Ok(mut cache) = self.members.lock() {
            cache.insert(class.to_string(), named.clone());
        }

        named
    }

    /// How this project writes one class, for the places that name a single one
    /// — hover, and a did-you-mean.
    pub fn element(&self, config: &Config, class: &str) -> String {
        self.elements(config)
            .iter()
            .find(|named| named.canonical == class)
            .map(|named| named.written.clone())
            // A class that reaches nothing — it lost a collision — has no
            // spelling in this project. Its own name is the least wrong thing
            // to show, and the compiler's diagnostic says the rest.
            .unwrap_or_else(|| class.to_string())
    }

    /// How this project writes one member of `class`.
    pub fn member(&self, config: &Config, class: &str, canonical: &str) -> String {
        self.members(config, class)
            .into_iter()
            .find(|named| named.canonical == canonical)
            .map(|named| named.written)
            .unwrap_or_else(|| canonical.to_string())
    }
}

/// The spellings to try for a name, best first.
///
/// The scheme's form is what a project with `all` expects; the canonical is the
/// fallback for the two cases where the scheme does not apply after all — an
/// identity entry that opted out, and no scheme at all.
fn spellings(alias: &str, canonical: &str, casing: Casing) -> Vec<String> {
    if alias != canonical {
        // An explicit rename beats the scheme and cannot collide with it: the
        // compiler removes such a class from the blanket pass entirely.
        return vec![alias.to_string()];
    }

    if casing == Casing::Pascal {
        return vec![canonical.to_string()];
    }

    vec![casing.apply(canonical), canonical.to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        Config::parse(&format!("[factory]\ncreate = \"create\"\n\n{text}")).expect("config")
    }

    fn written_element(config: &Config, class: &str) -> String {
        Vocabulary::default().element(config, class)
    }

    #[test]
    fn without_a_scheme_a_class_is_spelled_as_roblox_spells_it() {
        let config = config("");
        assert_eq!(written_element(&config, "TextLabel"), "TextLabel");
        assert_eq!(written_element(&config, "UICorner"), "UICorner");
    }

    #[test]
    fn a_scheme_renames_every_class() {
        let config = config("[elements]\nall = \"camelCase\"\n");
        assert_eq!(written_element(&config, "TextLabel"), "textLabel");
        // Word boundaries come from the canonical spelling, so `UI` stays whole.
        assert_eq!(written_element(&config, "UICorner"), "uiCorner");

        let snake = config_snake();
        assert_eq!(written_element(&snake, "UICorner"), "ui_corner");
    }

    fn config_snake() -> Config {
        config("[elements]\nall = \"snake_case\"\n")
    }

    #[test]
    fn an_explicit_entry_beats_the_scheme() {
        let config = config("[elements]\nall = \"camelCase\"\nTextLabel = \"text\"\n");
        assert_eq!(written_element(&config, "TextLabel"), "text");
        // And the rest of the project still follows the scheme.
        assert_eq!(written_element(&config, "UICorner"), "uiCorner");
    }

    /// A class can opt back out of a blanket rename by naming itself, and then
    /// the canonical spelling is the one that works.
    #[test]
    fn an_identity_entry_opts_a_class_out_of_the_scheme() {
        let config = config("[elements]\nall = \"camelCase\"\nTextLabel = \"TextLabel\"\n");
        assert_eq!(written_element(&config, "TextLabel"), "TextLabel");
        assert_eq!(written_element(&config, "UICorner"), "uiCorner");
    }

    /// Whether the compiler accepts `written` as this class.
    ///
    /// `Ok(None)` is not a failure: it is the compiler saying "not an alias,
    /// resolve normally", which is the right answer for a class no rename
    /// touched. Only a name that resolves *elsewhere*, or is retired, is wrong.
    fn accepted(config: &Config, named: &Named) -> bool {
        match config.resolve_element(&named.written) {
            Ok(Some(class)) => class == named.canonical,
            Ok(None) => named.written == named.canonical && roblox::is_class(&named.written),
            Err(_) => false,
        }
    }

    /// The property worth asserting (lsp-update.md): every name offered is one
    /// the compiler then accepts.
    #[test]
    fn every_element_offered_round_trips() {
        for scheme in ["", "PascalCase", "camelCase", "snake_case", "flatcase"] {
            let config = match scheme.is_empty() {
                true => config(""),
                false => config(&format!("[elements]\nall = \"{scheme}\"\n")),
            };

            let vocabulary = Vocabulary::default();
            let elements = vocabulary.elements(&config);
            assert!(elements.len() > 300, "{scheme}: {} elements", elements.len());

            for named in elements {
                assert!(accepted(&config, named), "{scheme}: {named:?} is not accepted");
            }
        }
    }

    #[test]
    fn every_member_offered_round_trips() {
        for scheme in ["", "camelCase", "snake_case", "flatcase"] {
            let config = match scheme.is_empty() {
                true => config(""),
                false => config(&format!("[properties]\nall = \"{scheme}\"\n")),
            };

            let vocabulary = Vocabulary::default();

            for class in ["Frame", "TextLabel", "TextButton", "UICorner"] {
                let members = vocabulary.members(&config, class);
                // Even the smallest of these inherits `Instance`, so a class
                // that came back nearly empty means the filter ate the list.
                assert!(members.len() > 15, "{scheme} {class}: {} members", members.len());

                for named in &members {
                    assert_eq!(
                        config.resolve_property(class, &named.written).as_deref(),
                        Ok(named.canonical),
                        "{scheme} {class}: {named:?} is not what the compiler resolves",
                    );
                }
            }
        }
    }

    /// `ChildAdded` and `childAdded` both live on `Instance` and collapse onto
    /// one spelling under any scheme. Offering both would show two identical
    /// entries meaning different things.
    #[test]
    fn a_collision_is_offered_once_and_resolves_to_the_modern_name() {
        let config = config("[properties]\nall = \"snake_case\"\n");
        let members = Vocabulary::default().members(&config, "Frame");

        let collided: Vec<&Named> =
            members.iter().filter(|named| named.written == "child_added").collect();

        assert_eq!(collided.len(), 1, "{collided:?}");
        assert_eq!(collided[0].canonical, "ChildAdded");
        assert!(roblox::is_deprecated("childAdded"));
    }

    #[test]
    fn no_two_members_share_a_spelling() {
        let config = config("[properties]\nall = \"flatcase\"\n");
        let members = Vocabulary::default().members(&config, "TextButton");

        let mut seen = std::collections::HashSet::new();
        for named in &members {
            assert!(seen.insert(named.written.clone()), "{} offered twice", named.written);
        }
    }

    #[test]
    fn a_property_alias_still_beats_the_scheme() {
        let config = config(
            "[properties]\nall = \"snake_case\"\n\n[properties.Frame]\nBackgroundColor3 = \"bg\"\n",
        );
        let members = Vocabulary::default().members(&config, "Frame");

        let names: Vec<&str> = members.iter().map(|named| named.written.as_str()).collect();
        assert!(names.contains(&"bg"), "the alias is missing");
        assert!(!names.contains(&"background_color3"), "the scheme's form is still offered");
        // And an unclaimed property still follows the scheme.
        assert!(names.contains(&"background_transparency"), "{names:?}");
    }
}
