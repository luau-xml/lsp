//! Roblox member types and documentation.
//!
//! The compiler's tables answer *whether* `MouseEnter` exists on a class, which
//! is all compiling needs. Editing wants more: what type it is, what it does,
//! and — for an event — what a handler is handed.
//!
//! That comes from the same two files this server already passes to luau-lsp
//! (see [`crate::proxy`]): the Roblox type definitions and the API docs the
//! luau-lsp extension downloads. Reading them rather than shipping our own copy
//! is what keeps our tooltips and luau-lsp's from disagreeing about the same
//! member — and means there is one thing to keep up to date, not two.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// A member as the definitions declare it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The class that declares it, which is often an ancestor of the one asked
    /// about — `MouseEnter` belongs to `GuiObject`, not `TextButton`.
    pub declared_by: String,
    /// Its Luau type, verbatim: `Color3`, `RBXScriptSignal<(number, number)>`.
    pub luau_type: String,
}

impl Member {
    /// The parameter types an event hands its listener, if it is one.
    ///
    /// Two spellings are in the wild and a machine can have both, since the
    /// luau-lsp extension keeps one file per security level and only refreshes
    /// the one in use. Newer releases parenthesise the pack —
    /// `RBXScriptSignal<(number, number)>` — and older ones do not:
    /// `RBXScriptSignal<number, number>`. Stripping an optional wrapper and then
    /// splitting handles both, and a lone `RBXScriptSignal<InputObject>` falls
    /// out of the same rule.
    pub fn event_parameters(&self) -> Option<Vec<String>> {
        let inner = self.luau_type.strip_prefix("RBXScriptSignal<")?.strip_suffix('>')?.trim();

        let inner =
            inner.strip_prefix('(').and_then(|rest| rest.strip_suffix(')')).unwrap_or(inner);

        Some(split_top_level(inner))
    }
}

/// Splits `a, b<c, d>, e` on the commas that are not inside brackets.
fn split_top_level(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    let mut previous = ' ';

    for character in text.chars() {
        match character {
            '<' | '(' | '{' | '[' => depth += 1,
            // Not a bracket: `->` is one token, and counting its `>` as a
            // closer leaves the depth negative for the rest of the type, so no
            // later comma ever splits.
            '>' if previous == '-' => {}
            '>' | ')' | '}' | ']' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(current.trim().to_string());
                current.clear();
                previous = character;
                continue;
            }
            _ => {}
        }
        current.push(character);
        previous = character;
    }

    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }

    parts.retain(|part| !part.is_empty());
    parts
}

#[derive(Default)]
struct Class {
    extends: Option<String>,
    members: HashMap<String, String>,
}

#[derive(Default)]
pub struct Api {
    classes: HashMap<String, Class>,
    docs: HashMap<String, Documentation>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Documentation {
    #[serde(default)]
    pub documentation: String,
    #[serde(default)]
    pub learn_more_link: String,
}

impl Api {
    /// The type of `member` on `class`, walking up the inheritance chain.
    pub fn member(&self, class: &str, member: &str) -> Option<Member> {
        let mut current = Some(class);

        while let Some(name) = current {
            let class = self.classes.get(name)?;

            if let Some(luau_type) = class.members.get(member) {
                return Some(Member {
                    declared_by: name.to_string(),
                    luau_type: luau_type.clone(),
                });
            }

            current = class.extends.as_deref();
        }

        None
    }

    /// What Roblox's own documentation says about it.
    pub fn documentation(&self, declared_by: &str, member: &str) -> Option<&Documentation> {
        self.docs.get(&format!("@roblox/globaltype/{declared_by}.{member}"))
    }

    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }

    fn read(definitions: &Path, docs: Option<&Path>) -> Self {
        let classes = std::fs::read_to_string(definitions)
            .map(|text| parse_definitions(&text))
            .unwrap_or_default();

        let docs = docs
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();

        Self { classes, docs }
    }
}

/// Parses `globalTypes.*.d.luau`.
///
/// ```text
/// declare extern type GuiObject extends GuiBase2d with
///     BackgroundColor3: Color3
///     MouseEnter: RBXScriptSignal<(number, number)>
/// end
/// ```
///
/// Older releases spell the header `declare class X extends Y`, so both are
/// accepted. Anything unrecognised is skipped rather than guessed at: a member
/// this misses simply has no type to show, which is where we started.
fn parse_definitions(text: &str) -> HashMap<String, Class> {
    let mut classes: HashMap<String, Class> = HashMap::new();
    let mut current: Option<(String, Class)> = None;

    for line in text.lines() {
        if line.starts_with("end") {
            if let Some((name, class)) = current.take() {
                classes.insert(name, class);
            }
            continue;
        }

        if let Some((name, extends)) = class_header(line) {
            if let Some((name, class)) = current.take() {
                classes.insert(name, class);
            }
            current = Some((name, Class { extends, members: HashMap::new() }));
            continue;
        }

        let Some((_, class)) = current.as_mut() else { continue };
        let Some((name, luau_type)) = member(line) else { continue };

        class.members.insert(name, luau_type);
    }

    if let Some((name, class)) = current {
        classes.insert(name, class);
    }

    classes
}

fn class_header(line: &str) -> Option<(String, Option<String>)> {
    let rest = line
        .strip_prefix("declare extern type ")
        .or_else(|| line.strip_prefix("declare class "))?;

    let mut words = rest.split_whitespace();
    let name = words.next()?.to_string();

    let extends = match words.next() {
        Some("extends") => words.next().map(str::to_string),
        _ => None,
    };

    Some((name, extends))
}

/// `\tName: Type` — indented, and a plain identifier before the colon.
fn member(line: &str) -> Option<(String, String)> {
    let body = line.strip_prefix(['\t', ' '])?.trim();
    let (name, luau_type) = body.split_once(':')?;

    let name = name.trim();
    let usable = !name.is_empty()
        && name.starts_with(|c: char| c == '_' || c.is_ascii_alphabetic())
        && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric());

    usable.then(|| (name.to_string(), luau_type.trim().to_string()))
}

/// The definitions this process reads, chosen once from the user's settings.
static API: OnceLock<Api> = OnceLock::new();

/// Points this process at a definitions file. Ignored if one is already loaded —
/// the settings behind it do not change without a restart.
pub fn install(definitions: Option<PathBuf>, docs: Option<PathBuf>) {
    let Some(definitions) = definitions else { return };
    let _ = API.set(Api::read(&definitions, docs.as_deref()));
}

/// What has been installed, or nothing.
///
/// Nothing is an ordinary state — a machine without the luau-lsp extension has
/// no definitions to read — and every caller degrades to the names alone.
pub fn global() -> Option<&'static Api> {
    API.get().filter(|api| !api.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFINITIONS: &str = "\
declare extern type Instance with
\tName: string
\tChanged: RBXScriptSignal<string>
end

declare extern type GuiBase2d extends Instance with
\tAutoLocalize: boolean
end

declare extern type GuiObject extends GuiBase2d with
\tBackgroundColor3: Color3
\tMouseEnter: RBXScriptSignal<(number, number)>
\tInputBegan: RBXScriptSignal<InputObject>
\tSelectionChanged: RBXScriptSignal<(boolean, GuiObject, GuiObject)>
\tActivatedNothing: RBXScriptSignal<()>
end

declare extern type TextLabel extends GuiObject with
\tText: string
end
";

    fn api() -> Api {
        Api { classes: parse_definitions(DEFINITIONS), docs: HashMap::new() }
    }

    #[test]
    fn a_members_type_comes_from_wherever_it_is_declared() {
        let api = api();

        let text = api.member("TextLabel", "Text").expect("Text");
        assert_eq!((text.declared_by.as_str(), text.luau_type.as_str()), ("TextLabel", "string"));

        // Three classes up, which is where the useful ones usually live.
        let mouse = api.member("TextLabel", "MouseEnter").expect("MouseEnter");
        assert_eq!(mouse.declared_by, "GuiObject");
        assert_eq!(mouse.luau_type, "RBXScriptSignal<(number, number)>");

        // All the way to the root.
        assert_eq!(api.member("TextLabel", "Name").expect("Name").declared_by, "Instance");
    }

    #[test]
    fn an_unknown_member_or_class_has_no_type() {
        let api = api();
        assert_eq!(api.member("TextLabel", "Nonsense"), None);
        assert_eq!(api.member("NotAClass", "Text"), None);
    }

    #[test]
    fn an_events_parameters_are_recovered_from_its_signal_type() {
        let api = api();
        let parameters =
            |class: &str, member: &str| api.member(class, member).expect(member).event_parameters();

        assert_eq!(
            parameters("TextLabel", "MouseEnter"),
            Some(vec!["number".into(), "number".into()])
        );
        // A single parameter is written without the parentheses.
        assert_eq!(parameters("TextLabel", "InputBegan"), Some(vec!["InputObject".into()]));
        assert_eq!(parameters("TextLabel", "ActivatedNothing"), Some(vec![]));
        // A property is not an event.
        assert_eq!(parameters("TextLabel", "Text"), None);
    }

    #[test]
    fn nested_generics_do_not_split_on_their_own_commas() {
        assert_eq!(split_top_level("number, number"), ["number", "number"]);
        assert_eq!(
            split_top_level("Map<string, number>, boolean"),
            ["Map<string, number>", "boolean"]
        );
        assert_eq!(split_top_level("(a, b) -> c, d"), ["(a, b) -> c", "d"]);
    }

    #[test]
    fn the_older_header_spelling_still_parses() {
        let classes =
            parse_definitions("declare class Frame extends GuiObject\n\tVisible: boolean\nend\n");
        assert_eq!(classes["Frame"].extends.as_deref(), Some("GuiObject"));
        assert_eq!(classes["Frame"].members["Visible"], "boolean");
    }

    #[test]
    fn methods_and_oddities_are_skipped_rather_than_guessed_at() {
        let classes = parse_definitions(
            "declare extern type X with\n\tFindFirstChild: (self: X, name: string) -> Instance?\n\t[\"Odd Name\"]: number\n\tPlain: number\nend\n",
        );

        // A method has a type like anything else, and reads fine.
        assert_eq!(classes["X"].members["FindFirstChild"], "(self: X, name: string) -> Instance?");
        // A bracketed key is not an attribute anyone can write in LuauX.
        assert!(!classes["X"].members.contains_key("[\"Odd Name\"]"));
        assert_eq!(classes["X"].members["Plain"], "number");
    }

    #[test]
    fn documentation_is_keyed_by_the_declaring_class() {
        let api = Api {
            classes: parse_definitions(DEFINITIONS),
            docs: HashMap::from([(
                "@roblox/globaltype/GuiObject.MouseEnter".to_string(),
                Documentation {
                    documentation: "Fires when a user moves their mouse in.".into(),
                    learn_more_link: "https://example.invalid".into(),
                },
            )]),
        };

        let member = api.member("TextLabel", "MouseEnter").expect("MouseEnter");
        let docs = api.documentation(&member.declared_by, "MouseEnter").expect("docs");
        assert!(docs.documentation.contains("moves their mouse"));
    }

    /// Against every definitions file this machine actually has.
    ///
    /// Not just one: the luau-lsp extension keeps a file per security level and
    /// refreshes only the one in use, so a machine routinely holds two different
    /// spellings at once. Testing whichever `read_dir` happened to return first
    /// is how the parenthesised-pack difference got missed.
    #[test]
    fn every_definitions_file_on_this_machine_parses() {
        let Some(storage) = crate::proxy::luau_lsp_storage() else {
            eprintln!("skipped: the luau-lsp extension has downloaded no definitions here");
            return;
        };

        let files: Vec<PathBuf> = std::fs::read_dir(&storage)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("globalTypes."))
            })
            .collect();

        if files.is_empty() {
            eprintln!("skipped: no globalTypes file");
            return;
        }

        for definitions in files {
            let what = definitions.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let api = Api::read(&definitions, None);
            assert!(!api.is_empty(), "{what} parsed to nothing");

            // The case that started this: an event three classes above where it
            // is written, with its real signature.
            let mouse = api.member("TextButton", "MouseEnter").unwrap_or_else(|| panic!("{what}"));
            assert_eq!(mouse.declared_by, "GuiObject", "{what}");
            assert_eq!(
                mouse.event_parameters(),
                Some(vec!["number".into(), "number".into()]),
                "{what}: {}",
                mouse.luau_type
            );

            assert_eq!(api.member("TextLabel", "Text").expect(&what).luau_type, "string", "{what}");
            // Inherited from GuiObject, and not an event.
            let background = api.member("Frame", "BackgroundColor3").expect(&what);
            assert_eq!(background.luau_type, "Color3", "{what}");
            assert_eq!(background.event_parameters(), None, "{what}");
        }
    }
}
