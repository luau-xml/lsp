//! Hover, and go-to-definition on a tag.
//!
//! Inside a captured expression there is nothing to say that luau-lsp does not
//! say better, so those forward. On a tag we know something it cannot: whether
//! `<Button>` is a Roblox class or a component defined on line 3 — the very
//! distinction the string `"Button"` in the generated code destroys.

use crate::bindings;
use crate::document::Document;
use crate::project::Project;
use crate::scan::{self, Context};
use crate::tree;
use luaux::roblox;
use serde_json::{json, Value};

pub enum Answer {
    Ours(Value),
    /// Verbatim Luau; luau-lsp has the types.
    Forward,
    /// Ours, and luau-lsp's too where it has something to add — a component tag,
    /// which is a real Luau identifier as well as a tag. Its type and the doc
    /// comment above its binding are luau-lsp's to know; where the tag resolved
    /// is ours. If the position does not map, or there is no child, ours stands
    /// on its own.
    ///
    /// `at` is the offset to *ask* about, which is not always the one under the
    /// cursor: `</Row>` names the same component as `<Row>`, but an element is
    /// emitted once, so only the opening name exists in the generated code.
    Both {
        ours: Value,
        at: usize,
    },
    Nothing,
}

pub fn hover(document: &Document, project: &Project, offset: usize) -> Answer {
    if forwards(&document.text, offset) {
        return Answer::Forward;
    }

    if let Some(tag) = tag_at(document, offset) {
        let ours = json!({
            "contents": { "kind": "markdown", "value": describe(document, project, &tag.name) },
            "range": tag.range,
        });

        // A class is a string in the generated code and luau-lsp has nothing to
        // say about it; a component is an identifier and it has everything.
        return match is_intrinsic(project, &tag.name) {
            true => Answer::Ours(ours),
            false => Answer::Both { ours, at: tag.declared_at },
        };
    }

    match attribute_at(document, project, offset) {
        Some(hover) => Answer::Ours(hover),
        None => Answer::Nothing,
    }
}

/// An attribute name: its type, where it comes from, and what it does.
///
/// The name alone is the least useful thing we know about it. `MouseEnter` is
/// an event on `GuiObject` that hands a listener two numbers, and all three
/// facts are in files this server already reads.
fn attribute_at(document: &Document, project: &Project, offset: usize) -> Option<Value> {
    let Context::AttributeName { tag, start, prefix } = scan::scan(&document.text, offset).context
    else {
        return None;
    };

    if prefix.is_empty() {
        return None;
    }

    // The scan reports the text *up to* the cursor, which is what completion
    // wants and the opposite of what hover does: nobody hovers the end of a
    // word. Read the whole name.
    let written = whole_name(&document.text, start);
    let prefix = written;

    // A component's props are its own business; only an intrinsic has an API.
    let class = match project.config.resolve_element(&tag) {
        Ok(Some(class)) => class.to_string(),
        Ok(None) if roblox::is_class(&tag) => tag.clone(),
        _ => return None,
    };

    let canonical = project.config.resolve_property(&class, &prefix).ok()?;
    let is_event = roblox::is_event(&class, &canonical);

    if !is_event && !roblox::has_property(&class, &canonical) {
        return None;
    }

    let api = crate::api::global();
    let known = api.and_then(|api| api.member(&class, &canonical));

    let mut text = match &known {
        Some(member) => format!("```luau\n{canonical}: {}\n```", member.luau_type),
        // No definitions on this machine. The name and its kind is still more
        // than nothing, and is what we could always say.
        None => format!("```luau\n{canonical}\n```"),
    };

    let declared_by = known.as_ref().map(|member| member.declared_by.as_str()).unwrap_or(&class);
    let what = if is_event { "Event" } else { "Property" };
    text.push_str(&format!("\n\n{what} of `{declared_by}`"));

    if canonical != prefix {
        text.push_str(&format!(", written `{prefix}` in this project"));
    }
    text.push('.');

    // What a handler is handed — the question a person asks right before
    // writing one.
    if let Some(parameters) = known.as_ref().and_then(crate::api::Member::event_parameters) {
        text.push_str(&format!("\n\nConnect a function of `({})`.", parameters.join(", ")));
    }

    if let Some(docs) =
        known.as_ref().and_then(|member| api?.documentation(&member.declared_by, &canonical))
    {
        text.push_str("\n\n---\n\n");
        text.push_str(&crate::completion::html_to_markdown(&docs.documentation));

        if !docs.learn_more_link.is_empty() {
            text.push_str(&format!("\n\n[Learn more]({})", docs.learn_more_link));
        }
    }

    Some(json!({
        "contents": { "kind": "markdown", "value": text },
        "range": document.range_at(start, start + prefix.len()),
    }))
}

/// Go-to-definition for a tag: an intrinsic has none, a component has its
/// binding.
pub fn definition(document: &Document, project: &Project, offset: usize) -> Answer {
    if forwards(&document.text, offset) {
        return Answer::Forward;
    }

    let Some(tag) = tag_at(document, offset) else {
        return Answer::Nothing;
    };

    // A class is not defined anywhere in this project, and sending someone to
    // the nearest binding of the same name would be a lie.
    if is_intrinsic(project, &tag.name) {
        return Answer::Nothing;
    }

    // A dotted `<Foo.Bar/>` is bound through its root.
    let name = tag.name;
    let root = name.split('.').next().unwrap_or(&name);

    match bindings::find(&document.text, root) {
        Some((start, end)) => Answer::Ours(json!({
            "uri": document.uri,
            "range": document.range_at(start, end),
        })),
        None => Answer::Nothing,
    }
}

/// Whether this position is verbatim Luau and belongs to luau-lsp.
fn forwards(source: &str, offset: usize) -> bool {
    matches!(scan::scan(source, offset).context, Context::Luau | Context::Expression { .. })
}

/// An element name under the cursor.
struct TagAt {
    name: String,
    /// The name the cursor is on, which is what an answer is *about*.
    range: Value,
    /// Where that name exists as a Luau identifier — the opening tag, since an
    /// element is emitted once however many times it is written.
    declared_at: usize,
}

/// The element name under the cursor, from the parsed tree where possible and
/// from the tolerant scan when the file does not parse.
fn tag_at(document: &Document, offset: usize) -> Option<TagAt> {
    let source = &document.text;

    for tag in tree::flatten(&tree::tree(source)) {
        if let Some((start, end)) = tag.name_at(offset) {
            return Some(TagAt {
                name: tag.name.clone(),
                range: document.range_at(start, end),
                declared_at: tag.open_name.0,
            });
        }
    }

    // Half-typed: `<Fra` has no tree yet but is still a tag name. There is no
    // pair to consult, so the name is its own best guess — and a file in this
    // state rarely compiles, in which case nothing forwards anyway.
    match scan::scan(source, offset).context {
        Context::TagName { start, prefix, .. } if !prefix.is_empty() => {
            let end = start + prefix.len();
            Some(TagAt { name: prefix, range: document.range_at(start, end), declared_at: start })
        }
        _ => None,
    }
}

/// The whole identifier beginning at `start`.
fn whole_name(source: &str, start: usize) -> String {
    let end = source[start..]
        .find(|c: char| c != '_' && !c.is_ascii_alphanumeric())
        .map(|offset| start + offset)
        .unwrap_or(source.len());

    source[start..end].to_string()
}

fn is_intrinsic(project: &Project, written: &str) -> bool {
    if written.contains('.') {
        return false;
    }

    match project.config.resolve_element(written) {
        Ok(Some(_)) => true,
        Ok(None) => roblox::is_class(written),
        // Retired by an alias: not a class you may write, so not an intrinsic.
        Err(_) => false,
    }
}

fn describe(document: &Document, project: &Project, written: &str) -> String {
    let canonical = match project.config.resolve_element(written) {
        Ok(Some(class)) => Some(class.to_string()),
        Ok(None) if roblox::is_class(written) => Some(written.to_string()),
        _ => None,
    };

    if let Some(class) = canonical {
        let mut text = format!("```luau\n{class}\n```\n\nRoblox class");

        if class != written {
            text.push_str(&format!(", written `{written}` in this project"));
        }
        if let Some(parent) = roblox::superclass(&class) {
            text.push_str(&format!("\n\nInherits `{parent}`."));
        }
        if roblox::has_text_property(&class) {
            text.push_str("\n\nText between the tags becomes its `Text` property.");
        }

        return text;
    }

    let root = written.split('.').next().unwrap_or(written);

    match bindings::find(&document.text, root) {
        Some((start, _)) => {
            let line = document.index.line_of(start) + 1;
            format!("```luau\n{written}\n```\n\nComponent, bound on line {line}.")
        }
        // Neither a class nor bound. The compiler reports this as an error; the
        // hover says the same thing rather than inventing a description.
        None => {
            format!("```luau\n{written}\n```\n\nNot a Roblox class, and not bound in this file.")
        }
    }
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

    fn document(text: &str) -> Document {
        Document::new("file:///a.luaux".into(), 1, text.into())
    }

    fn hover_text(marked: &str, config: &str) -> Option<String> {
        let cursor = marked.find('|').expect("a cursor marker");
        let document = document(&marked.replace('|', ""));

        match hover(&document, &project(config), cursor) {
            // `Both` still carries our text; it just lets luau-lsp add to it.
            Answer::Ours(value) | Answer::Both { ours: value, .. } => {
                Some(value["contents"]["value"].as_str()?.to_string())
            }
            _ => None,
        }
    }

    #[test]
    fn a_class_says_it_is_one() {
        let text = hover_text("local e = <Fra|me/>", "").expect("hover");
        assert!(text.contains("Roblox class"), "{text}");
        assert!(text.contains("Inherits `GuiObject`"), "{text}");
    }

    #[test]
    fn a_dotted_name_is_always_a_component() {
        let text = hover_text("local Foo = f()\nlocal e = <Foo.Ba|r/>", "").expect("hover");
        assert!(text.contains("Component"), "{text}");
    }

    #[test]
    fn a_text_bearing_class_says_what_text_children_do() {
        let text = hover_text("local e = <Text|Label>Hi</TextLabel>", "").expect("hover");
        assert!(text.contains("`Text` property"), "{text}");

        let frame = hover_text("local e = <Fra|me/>", "").expect("hover");
        assert!(!frame.contains("`Text` property"), "{frame}");
    }

    #[test]
    fn a_component_says_where_it_came_from() {
        let text =
            hover_text("local Row = require('./Row')\nlocal e = <Ro|w/>", "").expect("hover");
        assert!(text.contains("Component"), "{text}");
        assert!(text.contains("line 1"), "{text}");
    }

    /// A component tag is a Luau identifier as well as a tag, so luau-lsp gets
    /// asked too — its type and the doc comment above the binding are things
    /// this server cannot work out. A class is a string in the generated code
    /// and there is nothing there to ask about.
    #[test]
    fn a_component_also_asks_luau_lsp_and_a_class_does_not() {
        let component = document("local Row = require('./Row')\nlocal e = <Row/>");
        let at = "local Row = require('./Row')\nlocal e = <R".len();
        assert!(matches!(hover(&component, &project(""), at), Answer::Both { .. }));

        let class = document("local e = <Frame/>");
        let at = "local e = <Fra".len();
        assert!(matches!(hover(&class, &project(""), at), Answer::Ours(_)));
    }

    #[test]
    fn an_alias_reports_the_class_it_stands_for() {
        let text =
            hover_text("local e = <te|xt/>", "[elements]\nTextLabel = \"text\"\n").expect("hover");
        assert!(text.contains("TextLabel"), "{text}");
        assert!(text.contains("written `text`"), "{text}");
    }

    #[test]
    fn an_unresolvable_tag_says_so_rather_than_guessing() {
        let text = hover_text("local e = <Recei|pt/>", "").expect("hover");
        assert!(text.contains("not bound"), "{text}");
    }

    #[test]
    fn a_half_typed_tag_still_hovers() {
        let text = hover_text("local e = <Fra|", "").expect("hover");
        assert!(text.contains("not bound"), "{text}");
    }

    /// Available only where the luau-lsp extension has downloaded them.
    fn with_types() -> bool {
        let Some(storage) = crate::proxy::luau_lsp_storage() else { return false };
        let Some(definitions) = std::fs::read_dir(&storage)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("globalTypes."))
            })
        else {
            return false;
        };

        crate::api::install(Some(definitions), None);
        crate::api::global().is_some()
    }

    #[test]
    fn an_event_hover_gives_its_signature_and_what_a_handler_takes() {
        if !with_types() {
            eprintln!("skipped: no Roblox definitions on this machine");
            return;
        }

        let text = hover_text("local e = <TextButton Mouse|Enter={f}/>", "").expect("hover");

        // The signal type, in whichever of the two spellings this machine's
        // definitions use.
        assert!(text.contains("MouseEnter: RBXScriptSignal<"), "{text}");
        // Declared three classes above where it is written.
        assert!(text.contains("Event of `GuiObject`"), "{text}");
        // And the shape of a handler, which is the question asked right before
        // writing one.
        assert!(text.contains("function of `(number, number)`"), "{text}");
    }

    #[test]
    fn a_property_hover_gives_its_type() {
        if !with_types() {
            eprintln!("skipped: no Roblox definitions on this machine");
            return;
        }

        let text = hover_text("local e = <TextLabel Te|xt='hi'/>", "").expect("hover");
        assert!(text.contains("Text: string"), "{text}");
        assert!(text.contains("Property of `TextLabel`"), "{text}");
    }

    #[test]
    fn an_aliased_attribute_hover_names_both_spellings() {
        if !with_types() {
            eprintln!("skipped: no Roblox definitions on this machine");
            return;
        }

        let text = hover_text(
            "local e = <Frame bgCol|or={c}/>",
            "[properties.Frame]\nBackgroundColor3 = \"bgColor\"\n",
        )
        .expect("hover");

        assert!(text.contains("BackgroundColor3"), "{text}");
        assert!(text.contains("written `bgColor`"), "{text}");
    }

    #[test]
    fn an_attribute_a_class_does_not_have_hovers_to_nothing() {
        // The compiler already reports it as an error; inventing a description
        // would dress a mistake up as a fact.
        assert_eq!(hover_text("local e = <Frame Nonsen|se={x}/>", ""), None);
        // And a component's props are its own business.
        assert_eq!(hover_text("local Row = f()\nlocal e = <Row Any|thing={x}/>", ""), None);
    }

    #[test]
    fn expressions_and_luau_are_forwarded() {
        let cursor = "local e = <Frame Size={si".len();
        let inside = document("local e = <Frame Size={size}/>");
        assert!(matches!(hover(&inside, &project(""), cursor), Answer::Forward));

        let plain = document("local x = 1");
        assert!(matches!(hover(&plain, &project(""), 7), Answer::Forward));
    }

    #[test]
    fn definition_goes_to_a_components_binding() {
        let source = "local Row = require('./Row')\nlocal e = <Row/>";
        let document = document(source);
        let cursor = source.rfind("Row").expect("use");

        let Answer::Ours(value) = definition(&document, &project(""), cursor) else {
            panic!("expected a location");
        };
        assert_eq!(value["range"]["start"]["line"], json!(0));
        assert_eq!(value["range"]["start"]["character"], json!(6));
    }

    #[test]
    fn definition_on_a_class_goes_nowhere() {
        // A class is not defined in this project, and the nearest same-named
        // local is not its definition.
        let source = "local e = <Frame/>";
        let document = document(source);
        let cursor = source.find("Frame").expect("tag");
        assert!(matches!(definition(&document, &project(""), cursor), Answer::Nothing));
    }
}
