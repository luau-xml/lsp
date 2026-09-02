//! Completion — the feature the whole project is for.
//!
//! | Trigger | Source |
//! | --- | --- |
//! | `<` and `<Fra` | the creatable Roblox classes, plus names bound in the file |
//! | `<TextLabel ` | that class's properties and events, alias-aware |
//! | `</` | the innermost open tag |
//! | inside `{ … }` | forwarded to luau-lsp |
//!
//! Everything here runs off [`crate::scan`], never the AST, so `<Fra` completes
//! while the file does not compile — which is the only state that matters, since
//! a file being typed is never finished.

use crate::api;
use crate::document::Document;
use crate::project::Project;
use crate::regions;
use crate::scan::{self, Context};
use luaux::resolve::{blank_luaux_regions, Resolver};
use luaux::roblox;
use serde_json::{json, Value};

/// LSP `CompletionItemKind`s, named so the numbers do not float free.
mod kind {
    pub const FUNCTION: u8 = 3;
    pub const PROPERTY: u8 = 10;
    pub const CLASS: u8 = 7;
    pub const EVENT: u8 = 23;
}

pub enum Completion {
    /// A `CompletionList` we answered ourselves.
    Ours(Value),
    /// Verbatim Luau — luau-lsp has the types, we do not.
    Forward,
    /// An attribute name on a **component**, whose props are Luau's to know.
    ///
    /// A component tag compiles to a call and its attributes to that call's
    /// table argument, so the props are the expected keys of that table — which
    /// this server cannot work out and luau-lsp can. It is asked, and its answer
    /// is rewritten into markup by [`props`], since `Name = x` and `Name={x}`
    /// are the same key spelled for different syntaxes.
    ComponentProps { start: usize, prefix: String },
    /// A member of a dotted tag name: `<App.|`.
    ///
    /// What `App` holds is Luau's to know, exactly as a component's props are —
    /// the tag compiles to `App.Header(…)`, so the question is a member
    /// completion on the generated call. Offering the class list here is worse
    /// than offering nothing: `<App.Accessory/>` is not a thing that exists.
    TagMembers { start: usize, prefix: String },
    /// A position with nothing to offer. Answered as an empty list rather than
    /// forwarded, so luau-lsp does not suggest Luau symbols inside markup.
    Nothing,
}

/// `snippets` is the client's `completionItem.snippetSupport`. Without it, a
/// snippet's placeholder syntax arrives in the document as literal text.
pub fn complete(
    document: &Document,
    project: &Project,
    offset: usize,
    snippets: bool,
) -> Completion {
    let source = &document.text;
    let scan = scan::scan(source, offset);

    match &scan.context {
        Context::Luau | Context::Expression { .. } => Completion::Forward,

        // A dot in the name means the list is not classes and not components
        // bound here — it is whatever the thing before the dot holds.
        Context::TagName { start, prefix, closing: false } if prefix.contains('.') => {
            Completion::TagMembers { start: *start, prefix: prefix.clone() }
        }
        Context::TagName { start, prefix, closing: false } => {
            Completion::Ours(tags(document, project, *start, prefix))
        }

        Context::TagName { start, prefix, closing: true } => match scan.open.last() {
            Some(open) => Completion::Ours(close_tag(document, *start, prefix, &open.name)),
            None => Completion::Nothing,
        },

        Context::AttributeName { tag, start, prefix } => {
            match resolve_class(project, tag) {
                // Not a class, so a component: its props are declared in Luau,
                // and the generated call is where they can be asked about.
                None => Completion::ComponentProps { start: *start, prefix: prefix.clone() },
                Some(class) => Completion::Ours(attributes(
                    document, project, &class, *start, prefix, snippets,
                )),
            }
        }

        // Text becomes a `Text` property, and an attribute string is an ordinary
        // Luau string. Neither has anything to offer.
        Context::Text { .. } | Context::AttributeValue { .. } => Completion::Nothing,
    }
}

/// The written tag for a class, honouring `[elements]` aliases.
///
/// Aliases are exclusive: once `TextLabel = "text"` is configured, `<text>` is
/// the spelling and `<TextLabel>` is an error, so only one of the two is ever
/// offered.
fn resolve_class(project: &Project, written: &str) -> Option<String> {
    match project.config.resolve_element(written) {
        Ok(Some(class)) => Some(class.to_string()),
        // Retired by an alias — not a class you may write.
        Err(_) => None,
        Ok(None) => roblox::is_class(written).then(|| written.to_string()),
    }
}

fn tags(document: &Document, project: &Project, start: usize, prefix: &str) -> Value {
    let mut items = Vec::new();
    let range = document.range_at(start, start + prefix.len());

    // Generated in the project's own spelling, never echoed from the Roblox
    // tables: under `[elements] all` the canonical name is an error, so offering
    // it would offer the one thing that cannot be written (lsp-update.md §1).
    for named in project.vocabulary.elements(&project.config) {
        let detail = match named.written == named.canonical {
            true => "Roblox class".to_string(),
            false => format!("Roblox class {}", named.canonical),
        };

        items.push(item(&named.written, kind::CLASS, &detail, "1", &range));
    }

    // Components come first: a project's own names are what someone is usually
    // reaching for, and there are three of them against nine hundred classes.
    for name in components(&document.text, project) {
        items.push(item(&name, kind::FUNCTION, "component", "0", &range));
    }

    list(items)
}

/// Names bound in the file that could be components.
///
/// Capitalisation is the filter, as it is in JSX. It is a convention rather than
/// a rule — LuauX resolves by binding, not by case — but offering every local
/// would bury the class list under `i`, `count` and `props`, and a completion
/// list nobody can find anything in is worse than a shorter one.
fn components(source: &str, project: &Project) -> Vec<String> {
    let blanked = blank_luaux_regions(source, &regions::spans(source));
    let resolver = Resolver::new(&blanked, project.config.clone());

    let mut names: Vec<String> = resolver
        .bound()
        .iter()
        .filter(|name| {
            name.chars().next().is_some_and(char::is_uppercase) && !roblox::is_class(name)
        })
        .cloned()
        .collect();

    names.sort();
    names
}

fn attributes(
    document: &Document,
    project: &Project,
    class: &str,
    start: usize,
    prefix: &str,
    snippets: bool,
) -> Value {
    let members = Members {
        class,
        range: document.range_at(start, start + prefix.len()),
        api: api::global(),
        snippets,
    };

    // Likewise for members, and with collisions already settled: `ChildAdded`
    // and `childAdded` share one spelling under any scheme, and offering both
    // would show two identical entries meaning different things.
    let items = project
        .vocabulary
        .members(&project.config, class)
        .into_iter()
        .map(|named| {
            let kind = match roblox::is_event(class, named.canonical) {
                true => kind::EVENT,
                false => kind::PROPERTY,
            };
            let group = match kind == kind::EVENT {
                true => "1",
                false => "0",
            };

            members.item(&named.written, named.canonical, kind, group)
        })
        .collect();

    list(items)
}

/// What every attribute item needs to know.
struct Members<'a> {
    class: &'a str,
    range: Value,
    api: Option<&'static crate::api::Api>,
    snippets: bool,
}

impl Members<'_> {
    /// A property or event, described as fully as the definitions allow.
    ///
    /// Without them this is what it always was — a name and the word
    /// "property". With them it is the member's actual type and Roblox's own
    /// description, which is the difference between a list of names and
    /// something you can read.
    fn item(&self, written: &str, canonical: &str, kind: u8, group: &str) -> Value {
        let (class, range, api) = (self.class, &self.range, self.api);
        // An event takes a function, so completing one writes the function —
        // with its parameters typed, which is what makes them answerable inside
        // the body no matter how the factory is typed.
        let function_snippet = self.snippets && kind == kind::EVENT;

        let known = api.and_then(|api| api.member(class, canonical));

        let detail = match &known {
            Some(member) => member.luau_type.clone(),
            None if kind == kind::EVENT => "event".to_string(),
            None => "property".to_string(),
        };

        let mut item = json!({
            "label": written,
            "kind": kind,
            "detail": detail,
            "sortText": format!("{group}{written}"),
            "textEdit": { "range": range, "newText": written },
        });

        if let Some(documentation) = known
            .as_ref()
            .and_then(|found| api.and_then(|api| api.documentation(&found.declared_by, canonical)))
        {
            item["documentation"] = json!({
                "kind": "markdown",
                "value": describe(known.as_ref(), documentation),
            });
        }

        // Only when the client said it understands snippets: one that does not
        // inserts the placeholder syntax as literal text.
        if function_snippet {
            if let Some(parameters) = known.as_ref().and_then(crate::api::Member::event_parameters)
            {
                item["insertTextFormat"] = json!(2);
                item["textEdit"] = json!({
                    "range": range,
                    "newText": format!("{written}={{{}}}", handler(&parameters)),
                });
            }
        }

        item
    }
}

/// `function(p1: number, p2: number) end`, as a snippet.
///
/// The definitions carry parameter *types* but no names — Roblox does not
/// publish them for what an event fires — so the names are placeholders, and
/// tab stops so the first thing you do is type over them. Inventing plausible
/// names would be worse: `x, y` is right for `MouseEnter` and wrong for most
/// of the rest.
fn handler(parameters: &[String]) -> String {
    let named: Vec<String> = parameters
        .iter()
        .enumerate()
        .map(|(index, luau_type)| format!("${{{}:p{}}}: {luau_type}", index + 1, index + 1))
        .collect();

    format!("function({}) ${} end", named.join(", "), parameters.len() + 1)
}

/// The type, then what Roblox says it does.
fn describe(member: Option<&crate::api::Member>, docs: &crate::api::Documentation) -> String {
    let mut text = String::new();

    if let Some(member) = member {
        text.push_str(&format!("```luau\n{}\n```\n\n", member.luau_type));
    }

    // The docs are HTML fragments — `<code>GuiObject</code>` and the like —
    // which markdown renders as invisible tags rather than as code.
    text.push_str(&html_to_markdown(&docs.documentation));

    if !docs.learn_more_link.is_empty() {
        text.push_str(&format!("\n\n[Learn more]({})", docs.learn_more_link));
    }

    text
}

/// Roblox writes its documentation with a few HTML tags in it. Shared with
/// [`crate::hover`], which shows the same text.
pub fn html_to_markdown(text: &str) -> String {
    text.replace("<code>", "`")
        .replace("</code>", "`")
        .replace("<b>", "**")
        .replace("</b>", "**")
        .replace("<i>", "*")
        .replace("</i>", "*")
        .replace("<br/>", "\n")
        .replace("<br>", "\n")
}

/// Luau's reserved words, which cannot be bare table keys.
///
/// Kept here rather than taken from the lexer, which folds keywords into `Name`
/// deliberately — only a handful matter to LuauX detection, and which ones are
/// *reserved* is a different question from which ones it needs to recognise.
/// The contextual keywords (`type`, `export`, `continue`) are absent on purpose:
/// they are legal identifiers, and legal keys.
const RESERVED: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "if", "in", "local",
    "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

fn writable_as_an_attribute(label: &str) -> bool {
    crate::scan::is_name(label) && !RESERVED.contains(&label)
}

/// luau-lsp's expected keys for a component's props table, as markup attributes.
///
/// Only the keys: everything else it offers at that position is the ordinary
/// Luau scope — `print`, `game`, every local — which is right for a table
/// constructor and wrong for an attribute list. It marks the expected ones with
/// `sortText` "1" and buckets the rest from "3" down, and that is the only
/// signal available. If it ever stops, this returns nothing rather than nine
/// hundred globals inside a tag; [`crate::server`] logs when a component yields
/// no props, so the failure is visible rather than merely quiet.
///
/// `range` covers the partial name already typed, as it does for an intrinsic —
/// an editor's own idea of a word does not survive `Foo.Bar` or a lowercase
/// alias.
pub fn props(items: &[Value], range: &Value, snippets: bool) -> Value {
    const EXPECTED_KEY: &str = "1";

    let rewritten = items
        .iter()
        .filter(|item| item.get("sortText").and_then(Value::as_str) == Some(EXPECTED_KEY))
        .filter_map(|item| {
            let label = item.get("label").and_then(Value::as_str)?;
            // A key that is not a plain name has no markup spelling at all:
            // `["data-x"]` cannot be written as an attribute, and `end` as one
            // would compile to `{ end = x }`, which is not Luau.
            if !writable_as_an_attribute(label) {
                return None;
            }

            let detail = item.get("detail").and_then(Value::as_str).unwrap_or("prop");
            // luau-lsp's own kind, rather than reading the type text back: a
            // prop that takes a function is completed as one, the way an event
            // is on an intrinsic.
            let takes_a_function =
                item.get("kind").and_then(Value::as_u64) == Some(kind::FUNCTION as u64);

            let mut written = json!({
                "label": label,
                "kind": if takes_a_function { kind::EVENT } else { kind::PROPERTY },
                "detail": detail,
                "sortText": format!("0{label}"),
                "textEdit": { "range": range, "newText": label },
            });

            if let Some(documentation) = item.get("documentation") {
                written["documentation"] = documentation.clone();
            }

            // The parameters are in the type, not in the definitions, so the
            // body is left empty rather than invented — unlike an intrinsic's
            // event, where Roblox publishes what a handler is handed.
            if takes_a_function && snippets {
                written["insertTextFormat"] = json!(2);
                written["textEdit"] =
                    json!({ "range": range, "newText": format!("{label}={{function() $1 end}}") });
            }

            Some(written)
        })
        .collect();

    list(rewritten)
}

/// The child's members of `App`, as tag names.
///
/// Rebuilt rather than relayed, for the same reason [`props`] rebuilds: the
/// child's `textEdit` is against the generated file, and the range that matters
/// here is the one segment of the tag name the cursor is in.
pub fn members(items: &[Value], range: &Value) -> Value {
    let rewritten = items
        .iter()
        .filter_map(|item| {
            let label = item.get("label").and_then(Value::as_str)?;

            // A key that is not a plain name cannot be written as a tag at all:
            // `App["my-thing"]` has no markup spelling.
            if !writable_as_an_attribute(label) {
                return None;
            }

            let detail = item.get("detail").and_then(Value::as_str).unwrap_or("member");

            let mut written = json!({
                "label": label,
                "kind": kind::FUNCTION,
                "detail": detail,
                "sortText": format!("0{label}"),
                "textEdit": { "range": range, "newText": label },
            });

            if let Some(documentation) = item.get("documentation") {
                written["documentation"] = documentation.clone();
            }

            Some(written)
        })
        .collect();

    list(rewritten)
}

/// `</` completes to whatever is actually open, which is the only useful answer.
fn close_tag(document: &Document, start: usize, prefix: &str, name: &str) -> Value {
    let end = start + prefix.len();
    let range = document.range_at(start, end);

    // Do not add a `>` that is already there.
    let closed = document.text[end..].trim_start().starts_with('>');
    let text = if closed { name.to_string() } else { format!("{name}>") };

    let label = if name.is_empty() { "</>" } else { name };

    list(vec![json!({
        "label": label,
        "kind": kind::CLASS,
        "detail": "close this element",
        "sortText": "0",
        "textEdit": { "range": range, "newText": text },
    })])
}

fn item(label: &str, kind: u8, detail: &str, group: &str, range: &Value) -> Value {
    json!({
        "label": label,
        "kind": kind,
        "detail": detail,
        // Grouped first, then alphabetical inside the group.
        "sortText": format!("{group}{label}"),
        // An explicit range rather than a prefix: `Foo.Bar` and lowercase aliases
        // both fall outside what an editor guesses a word to be.
        "textEdit": { "range": range, "newText": label },
    })
}

fn list(items: Vec<Value>) -> Value {
    json!({ "isIncomplete": false, "items": items })
}

#[cfg(test)]
mod tests {
    use super::*;
    use luaux::config::Config;
    use std::path::Path;

    fn project(config: &str) -> Project {
        let mut project = Project::discover(Path::new("/nonexistent-luaux-project/a.luaux"));
        project.config = Config::parse(config).expect("config");
        project
    }

    /// Completes at `|`.
    fn complete_at(marked: &str, config: &str) -> Completion {
        let cursor = marked.find('|').expect("a cursor marker");
        let document = Document::new("file:///a.luaux".into(), 1, marked.replace('|', ""));
        super::complete(&document, &project(config), cursor, false)
    }

    fn labels(marked: &str, config: &str) -> Vec<String> {
        match complete_at(marked, config) {
            Completion::Ours(list) => list["items"]
                .as_array()
                .expect("items")
                .iter()
                .map(|item| item["label"].as_str().unwrap_or_default().to_string())
                .collect(),
            Completion::Forward => panic!("forwarded"),
            Completion::ComponentProps { .. }
            | Completion::TagMembers { .. }
            | Completion::Nothing => Vec::new(),
        }
    }

    #[test]
    fn a_tag_offers_the_class_list() {
        let labels = labels("local e = <Fra|", "");
        assert!(labels.contains(&"Frame".to_string()));
        assert!(labels.contains(&"TextLabel".to_string()));
        // Not creatable, so not an element.
        assert!(!labels.contains(&"Players".to_string()));
    }

    #[test]
    fn a_bare_open_angle_offers_them_too() {
        assert!(labels("local e = <|", "").contains(&"Frame".to_string()));
    }

    #[test]
    fn bound_names_are_offered_as_components() {
        let labels = labels("local Receipt = require('./Receipt')\nlocal e = <|", "");
        assert!(labels.contains(&"Receipt".to_string()), "{labels:?}");
        // Lowercase locals are not components by convention, and there are a lot
        // of them.
        assert!(!labels.contains(&"require".to_string()));
    }

    #[test]
    fn an_alias_replaces_the_class_it_renamed() {
        let labels = labels("local e = <|", "[elements]\nTextLabel = \"text\"\n");
        assert!(labels.contains(&"text".to_string()), "{labels:?}");
        // Exclusive: offering the retired spelling would suggest an error.
        assert!(!labels.contains(&"TextLabel".to_string()), "{labels:?}");
    }

    #[test]
    fn attributes_come_from_the_class_and_its_ancestors() {
        let labels = labels("local e = <TextLabel Te|", "");
        assert!(labels.contains(&"Text".to_string()));
        // Inherited from GuiObject.
        assert!(labels.contains(&"BackgroundColor3".to_string()));
        // Read-only, so it cannot be set.
        assert!(!labels.contains(&"ContentText".to_string()));
    }

    #[test]
    fn events_are_offered_alongside_properties() {
        let labels = labels("local e = <TextButton |/>", "");
        assert!(labels.contains(&"Activated".to_string()), "{labels:?}");
    }

    /// `[elements] all` renames every class at once and retires the canonical
    /// spelling, so offering `TextLabel` there offers the one thing that is an
    /// error (lsp-update.md §1).
    #[test]
    fn a_casing_scheme_renames_every_tag() {
        let camel = labels("local e = <|", "[elements]\nall = \"camelCase\"\n");
        assert!(camel.contains(&"textLabel".to_string()), "{:?}", &camel[..5.min(camel.len())]);
        assert!(!camel.contains(&"TextLabel".to_string()), "the retired spelling is offered");
        // Word boundaries come from the canonical name: `UI` stays one word.
        assert!(camel.contains(&"uiCorner".to_string()));

        let snake = labels("local e = <|", "[elements]\nall = \"snake_case\"\n");
        assert!(snake.contains(&"ui_corner".to_string()), "{:?}", &snake[..5.min(snake.len())]);

        let flat = labels("local e = <|", "[elements]\nall = \"flatcase\"\n");
        assert!(flat.contains(&"uicorner".to_string()));
    }

    #[test]
    fn an_explicit_entry_still_beats_the_scheme() {
        let labels =
            labels("local e = <|", "[elements]\nall = \"camelCase\"\nTextLabel = \"text\"\n");
        assert!(labels.contains(&"text".to_string()), "the override is missing");
        // Neither the canonical nor the scheme's form: both are retired.
        assert!(!labels.contains(&"TextLabel".to_string()));
        assert!(!labels.contains(&"textLabel".to_string()));
        // And every other class still follows the scheme.
        assert!(labels.contains(&"uiCorner".to_string()));
    }

    #[test]
    fn a_casing_scheme_renames_every_attribute() {
        let labels = labels("local e = <TextLabel |/>", "[properties]\nall = \"snake_case\"\n");
        assert!(labels.contains(&"background_transparency".to_string()));
        assert!(!labels.contains(&"BackgroundTransparency".to_string()));
    }

    /// `ChildAdded` and `childAdded` both live on `Instance`, so they collapse
    /// onto one spelling under any scheme — and two identical entries meaning
    /// different things is worse than one (lsp-update.md §3).
    #[test]
    fn a_collided_member_is_offered_once() {
        let labels = labels("local e = <TextLabel |/>", "[properties]\nall = \"snake_case\"\n");
        let collided = labels.iter().filter(|label| *label == "child_added").count();
        assert_eq!(collided, 1, "{collided} entries for child_added");

        let mut seen = std::collections::HashSet::new();
        for label in &labels {
            assert!(seen.insert(label), "{label} offered twice");
        }
    }

    #[test]
    fn attribute_aliases_are_honoured() {
        let labels =
            labels("local e = <Frame |/>", "[properties.Frame]\nBackgroundColor3 = \"bgColor\"\n");
        assert!(labels.contains(&"bgColor".to_string()), "{labels:?}");
        assert!(!labels.contains(&"BackgroundColor3".to_string()), "{labels:?}");
    }

    /// The Roblox definitions are read from what the luau-lsp extension has
    /// downloaded, so these describe what is available rather than requiring it.
    fn with_types() -> bool {
        let Some(storage) = crate::proxy::luau_lsp_storage() else { return false };
        let files: Vec<_> = std::fs::read_dir(&storage)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("globalTypes."))
            })
            .collect();

        let Some(definitions) = files.into_iter().next() else { return false };
        crate::api::install(Some(definitions), None);
        crate::api::global().is_some()
    }

    fn item_for(marked: &str, label: &str, snippets: bool) -> Option<Value> {
        let cursor = marked.find('|').expect("a cursor marker");
        let document = Document::new("file:///a.luaux".into(), 1, marked.replace('|', ""));

        match super::complete(&document, &project(""), cursor, snippets) {
            Completion::Ours(list) => {
                list["items"].as_array()?.iter().find(|item| item["label"] == json!(label)).cloned()
            }
            _ => None,
        }
    }

    #[test]
    fn an_attribute_is_described_by_its_type() {
        if !with_types() {
            eprintln!("skipped: no Roblox definitions on this machine");
            return;
        }

        let event = item_for("local e = <TextButton |/>", "MouseEnter", false).expect("MouseEnter");
        // Two spellings of the signal type are in the wild and a machine can
        // hold both, so assert what they agree on rather than which file this
        // one happened to read.
        let detail = event["detail"].as_str().expect("detail");
        assert!(detail.starts_with("RBXScriptSignal<"), "{detail}");
        assert!(detail.contains("number"), "{detail}");

        let property = item_for("local e = <TextLabel |/>", "Text", false).expect("Text");
        assert_eq!(property["detail"], json!("string"));
    }

    #[test]
    fn completing_an_event_writes_the_handler_it_expects() {
        if !with_types() {
            eprintln!("skipped: no Roblox definitions on this machine");
            return;
        }

        let event = item_for("local e = <TextButton |/>", "MouseEnter", true).expect("MouseEnter");

        // A snippet, with the parameters typed so they can be asked about
        // inside the body whatever the factory's own types say.
        assert_eq!(event["insertTextFormat"], json!(2));
        assert_eq!(
            event["textEdit"]["newText"],
            json!("MouseEnter={function(${1:p1}: number, ${2:p2}: number) $3 end}")
        );

        // A property takes a value, not a handler.
        let property = item_for("local e = <TextLabel |/>", "Text", true).expect("Text");
        assert_eq!(property["textEdit"]["newText"], json!("Text"));
        assert!(property["insertTextFormat"].is_null());
    }

    #[test]
    fn a_client_without_snippet_support_gets_a_plain_name() {
        if !with_types() {
            eprintln!("skipped: no Roblox definitions on this machine");
            return;
        }

        // The placeholders would otherwise arrive as literal text.
        let event = item_for("local e = <TextButton |/>", "MouseEnter", false).expect("MouseEnter");
        assert_eq!(event["textEdit"]["newText"], json!("MouseEnter"));
        assert!(event["insertTextFormat"].is_null());
    }

    #[test]
    fn a_handler_is_shaped_by_its_parameters() {
        assert_eq!(handler(&[]), "function() $1 end");
        assert_eq!(handler(&["InputObject".to_string()]), "function(${1:p1}: InputObject) $2 end");
    }

    #[test]
    fn roblox_html_becomes_markdown() {
        // The docs are HTML fragments; markdown renders the tags as nothing.
        assert_eq!(
            html_to_markdown("Determines the <code>GuiObject</code> background <b>color</b>."),
            "Determines the `GuiObject` background **color**."
        );
    }

    /// A component's props are declared in Luau, so the question goes to
    /// luau-lsp at the call the tag becomes. This server never guesses them.
    #[test]
    fn a_component_asks_luau_lsp_for_its_props() {
        assert!(matches!(
            complete_at("local Row = f()\nlocal e = <Row |/>", ""),
            Completion::ComponentProps { .. }
        ));

        // An intrinsic is answered here, from the Roblox tables.
        assert!(matches!(complete_at("local e = <TextLabel |/>", ""), Completion::Ours(_)));
    }

    /// Only the expected keys. Everything else luau-lsp offers inside a table
    /// constructor is the ordinary Luau scope, which is right for Luau and
    /// nonsense as an attribute list.
    #[test]
    fn props_keep_the_expected_keys_and_drop_the_scope() {
        let range =
            json!({ "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } });
        let items = vec![
            json!({ "label": "Name", "kind": 5, "detail": "string", "sortText": "1" }),
            json!({ "label": "OnClick", "kind": 3, "detail": "() -> ()", "sortText": "1" }),
            json!({ "label": "print", "kind": 3, "detail": "(T...) -> ()", "sortText": "4" }),
            json!({ "label": "Row", "kind": 3, "sortText": "3" }),
        ];

        let list = props(&items, &range, false);
        let labels: Vec<&str> = list["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|i| i["label"].as_str())
            .collect();

        assert_eq!(labels, ["Name", "OnClick"]);
    }

    #[test]
    fn a_prop_that_takes_a_function_is_completed_as_one() {
        let range =
            json!({ "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 3 } });
        let items = vec![
            json!({ "label": "OnClick", "kind": 3, "detail": "() -> ()", "sortText": "1" }),
            json!({ "label": "Name", "kind": 5, "detail": "string", "sortText": "1" }),
        ];

        let list = props(&items, &range, true);
        let items = list["items"].as_array().expect("items");

        // The parameters are the component's own business — unlike a Roblox
        // event, nothing publishes what a handler is handed — so the body is
        // left empty rather than invented.
        assert_eq!(items[0]["textEdit"]["newText"], json!("OnClick={function() $1 end}"));
        assert_eq!(items[0]["insertTextFormat"], json!(2));
        // A value prop takes a value.
        assert_eq!(items[1]["textEdit"]["newText"], json!("Name"));
        assert!(items[1]["insertTextFormat"].is_null());
    }

    #[test]
    fn a_client_without_snippets_gets_a_plain_prop_name() {
        let range =
            json!({ "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } });
        let items =
            vec![json!({ "label": "OnClick", "kind": 3, "detail": "() -> ()", "sortText": "1" })];

        let list = props(&items, &range, false);
        assert_eq!(list["items"][0]["textEdit"]["newText"], json!("OnClick"));
    }

    /// A key that is not a plain name has no markup spelling at all.
    #[test]
    fn a_key_that_cannot_be_written_as_an_attribute_is_dropped() {
        let range =
            json!({ "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } });
        let items = vec![
            json!({ "label": "data-x", "kind": 5, "sortText": "1" }),
            json!({ "label": "end", "kind": 5, "sortText": "1" }),
            json!({ "label": "Name", "kind": 5, "sortText": "1" }),
        ];

        let labels: Vec<String> = props(&items, &range, false)["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|i| i["label"].as_str().unwrap_or_default().to_string())
            .collect();

        assert_eq!(labels, ["Name"]);
    }

    #[test]
    fn a_closing_tag_completes_what_is_open() {
        let labels = labels("local e = <Frame><TextLabel></|", "");
        assert_eq!(labels, ["TextLabel"]);
    }

    #[test]
    fn a_closing_tag_does_not_double_the_angle_bracket() {
        let Completion::Ours(list) = complete_at("local e = <Frame></|>", "") else {
            panic!("expected our own completions");
        };
        assert_eq!(list["items"][0]["textEdit"]["newText"], json!("Frame"));

        let Completion::Ours(list) = complete_at("local e = <Frame></|", "") else {
            panic!("expected our own completions");
        };
        assert_eq!(list["items"][0]["textEdit"]["newText"], json!("Frame>"));
    }

    #[test]
    fn expressions_and_plain_luau_are_forwarded() {
        assert!(matches!(complete_at("local e = <Frame Size={si|", ""), Completion::Forward));
        assert!(matches!(complete_at("local x = str|", ""), Completion::Forward));
    }

    #[test]
    fn text_and_attribute_strings_offer_nothing() {
        // Forwarding here would have luau-lsp suggest Luau symbols inside prose.
        assert!(matches!(
            complete_at("local e = <TextLabel>He|</TextLabel>", ""),
            Completion::Nothing
        ));
        assert!(matches!(complete_at("local e = <Frame Name=\"a|\"/>", ""), Completion::Nothing));
    }

    #[test]
    fn the_edit_replaces_the_partial_name() {
        let Completion::Ours(list) = complete_at("local e = <Fra|", "") else {
            panic!("expected our own completions");
        };

        let edit = &list["items"][0]["textEdit"]["range"];
        assert_eq!(edit["start"]["character"], json!(11));
        assert_eq!(edit["end"]["character"], json!(14));
    }
}
