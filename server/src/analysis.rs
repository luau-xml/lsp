//! Compiling on change: our diagnostics, and the output the proxy forwards.
//!
//! Two things come out of one compile. The diagnostics need nothing else — no
//! source map, no luau-lsp — which is why they land first and cheapest. The
//! generated `.luau` and its map are what every forwarded feature stands on.
//!
//! A file that does not compile still has to be useful. Our own features never
//! touch the AST (see [`crate::scan`]), and forwarding falls back to the last
//! successful compile — marked stale rather than passed off as current, and
//! dropped once the edit is big enough that the map cannot still be true.

use crate::document::Document;
use crate::map_builder;
use crate::project::Project;
use crate::scan::{self, Context};
use crate::sourcemap::SourceMap;
use luaux::backend::Vide;
use luaux::roblox;
use serde_json::{json, Value};

/// A compile that succeeded, and the map from it.
pub struct Compiled {
    pub output: String,
    pub map: SourceMap,
    /// Document version this came from.
    pub version: i64,
    /// Lines in the source it was built from. The map is only valid while line
    /// numbers still correspond, so a change to this count retires it outright.
    pub lines: usize,
}

pub struct Analysis {
    /// Ours, always describing the document as it is right now.
    pub diagnostics: Vec<Value>,
    /// The generated `.luau`, current or stale. `None` means nothing has ever
    /// compiled, and forwarding is simply unavailable.
    pub compiled: Option<Compiled>,
    /// Whether `compiled` describes an older revision of the document.
    pub stale: bool,
}

impl Analysis {
    /// Compiles `document`, reusing `previous` as a fallback if it does not.
    pub fn run(document: &Document, project: &Project, previous: Option<Compiled>) -> Self {
        let mut diagnostics = Vec::new();

        if let Some(error) = &project.error {
            // A broken `luaux.toml` changes what compiles, so it cannot be
            // quietly ignored — the build would disagree with us.
            diagnostics.push(json!({
                "range": document.range_at(0, 0),
                "severity": 1,
                "source": "luaux",
                "message": error,
            }));
        }

        let lines = document.index.line_count();

        // Recovering, not stopping at the first: a bad tag should cost its own
        // diagnostic, not the rest of the file's. It also keeps the generated
        // Luau, without which luau-lsp has nothing to check and every type error
        // in the file disappears along with the markup ones.
        match luaux::compile::compile_recovering(&document.text, &Vide, project.config.clone()) {
            Ok(compiled) => {
                for error in &compiled.errors {
                    diagnostics.push(diagnostic(
                        document,
                        project,
                        &error.message,
                        error.offset,
                        error.length,
                        error.help.as_deref(),
                        1,
                    ));
                }

                for warning in &compiled.warnings {
                    diagnostics.push(diagnostic(
                        document,
                        project,
                        &warning.message,
                        warning.offset,
                        warning.length,
                        warning.help.as_deref(),
                        2,
                    ));
                }

                let map = map_builder::build(&document.text, &compiled.output, &project.config);

                Self {
                    diagnostics,
                    compiled: Some(Compiled {
                        output: compiled.output,
                        map,
                        version: document.version,
                        lines,
                    }),
                    stale: false,
                }
            }
            // Only a parse error reaches here now, and there is genuinely no
            // output for one: the tree it would be built from does not exist.
            Err(error) => {
                diagnostics.push(diagnostic(
                    document,
                    project,
                    &error.message,
                    error.offset,
                    error.length,
                    error.help.as_deref(),
                    1,
                ));

                // Keep the last good output only while line numbers can still be
                // trusted. Line preservation is what makes the map meaningful, so
                // once the line count moves, a stale map is not stale — it is wrong.
                let compiled = previous.filter(|compiled| compiled.lines == lines);
                let stale = compiled.is_some();

                Self { diagnostics, compiled, stale }
            }
        }
    }
}

/// A [`CompileError`] or [`Warning`] as an LSP diagnostic.
///
/// `help` becomes related information rather than being glued onto the message:
/// it is a separate thought, and editors render it separately.
fn diagnostic(
    document: &Document,
    project: &Project,
    message: &str,
    offset: usize,
    length: usize,
    help: Option<&str>,
    severity: u8,
) -> Value {
    let start = crate::line_index::floor_boundary(&document.text, offset);
    // A zero length means "point here", but a zero-width range is invisible in
    // most editors, so it widens to the character it points at — the whole
    // character. Widening by one *byte* lands inside anything non-ASCII, and
    // `\u{a0}` is one Option+Space away on every Mac keyboard.
    let end = crate::line_index::ceil_boundary(&document.text, start + length.max(1));
    let range = document.range_at(start, end);

    let mut value = json!({
        "range": range,
        "severity": severity,
        "source": "luaux",
        "message": message,
    });

    if let Some(help) = help {
        value["relatedInformation"] = json!([{
            "location": { "uri": document.uri, "range": range },
            "message": help,
        }]);
    }

    // The compiler already computed the hard part of a did-you-mean; carrying the
    // candidates on the diagnostic means the code action does not have to read
    // the message back in English.
    if let Some(fix) = suggestion(document, project, start, end) {
        value["data"] = fix;
    }

    value
}

/// Replacement candidates for the text a diagnostic underlines.
///
/// Works from the source, not the message: what is underlined is either an
/// element name or an attribute name, and the Roblox tables answer both.
///
/// `closest_class` and `closest_members` answer in Roblox's spelling, and a
/// quick fix that inserts one would insert an error in a project with a casing
/// scheme — so every candidate is put back through the project's own vocabulary
/// before it is offered (lsp-update.md §2).
fn suggestion(document: &Document, project: &Project, start: usize, end: usize) -> Option<Value> {
    let text = &document.text;
    let vocabulary = &project.vocabulary;

    // `<Frmae` — the underline covers the `<` as well.
    if let Some(rest) = text.get(start..end).and_then(|slice| slice.strip_prefix('<')) {
        let name = rest.trim();
        let candidates: Vec<String> = roblox::closest_class(name)
            .into_iter()
            .map(|class| vocabulary.element(&project.config, class))
            .collect();

        if candidates.is_empty() {
            return None;
        }

        return Some(json!({
            "kind": "element",
            "range": document.range_at(start + 1, end),
            "candidates": candidates,
        }));
    }

    // An attribute name: the enclosing tag says which class to ask, and the tag
    // is in the project's spelling too, so it has to be resolved rather than
    // looked up as a Roblox name.
    let written = text.get(start..end)?;
    let Context::AttributeName { tag, .. } = scan::scan(text, start).context else {
        return None;
    };

    let class = match project.config.resolve_element(&tag) {
        Ok(Some(class)) => class.to_string(),
        Ok(None) if roblox::is_class(&tag) => tag.clone(),
        _ => return None,
    };

    let candidates: Vec<String> = roblox::closest_members(&class, written)
        .into_iter()
        .map(|member| vocabulary.member(&project.config, &class, member))
        .collect();

    if candidates.is_empty() {
        return None;
    }

    Some(json!({
        "kind": "member",
        "range": document.range_at(start, end),
        "candidates": candidates,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Project;
    use std::path::Path;

    fn analyse(text: &str) -> Analysis {
        let document = Document::new("file:///a.luaux".into(), 1, text.into());
        let project = Project::discover(Path::new("/nonexistent-luaux-project/a.luaux"));
        Analysis::run(&document, &project, None)
    }

    fn messages(analysis: &Analysis) -> Vec<String> {
        analysis
            .diagnostics
            .iter()
            .map(|d| d["message"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    #[test]
    fn a_clean_file_compiles_and_says_nothing() {
        let analysis = analyse("local create = f()\nlocal e = <Frame/>\n");
        assert!(analysis.diagnostics.is_empty(), "{:?}", messages(&analysis));
        assert!(analysis.compiled.is_some());
        assert!(!analysis.stale);
    }

    #[test]
    fn a_compile_error_becomes_a_diagnostic_at_its_offset() {
        let analysis = analyse("local create = f()\nlocal e = <Frmae/>\n");
        let diagnostic = analysis.diagnostics.first().expect("a diagnostic");

        assert_eq!(diagnostic["severity"], json!(1));
        assert_eq!(diagnostic["source"], json!("luaux"));
        assert_eq!(diagnostic["range"]["start"]["line"], json!(1));
        assert_eq!(diagnostic["range"]["start"]["character"], json!(10));
    }

    #[test]
    fn help_travels_as_related_information() {
        let analysis = analyse("local create = f()\nlocal e = <Frmae/>\n");
        let related = &analysis.diagnostics[0]["relatedInformation"][0]["message"];
        assert!(
            related.as_str().is_some_and(|text| text.contains("did you mean <Frame>")),
            "{related:?}"
        );
    }

    #[test]
    fn a_misspelt_element_carries_its_candidates() {
        let analysis = analyse("local create = f()\nlocal e = <Frmae/>\n");
        let data = &analysis.diagnostics[0]["data"];
        assert_eq!(data["kind"], json!("element"));
        assert_eq!(data["candidates"], json!(["Frame"]));
    }

    /// A quick fix inserts its candidate verbatim, so a candidate in Roblox's
    /// spelling would insert an error into a project with a scheme.
    #[test]
    fn a_did_you_mean_is_offered_in_the_projects_own_spelling() {
        let document = Document::new(
            "file:///a.luaux".into(),
            1,
            "local create = f()\nlocal e = <textLabl/>\n".into(),
        );
        let mut project = Project::discover(Path::new("/nonexistent-luaux-project/a.luaux"));
        project.config =
            luaux::config::Config::parse("[elements]\nall = \"camelCase\"\n").expect("config");

        let analysis = Analysis::run(&document, &project, None);
        let data = &analysis.diagnostics[0]["data"];

        assert_eq!(data["kind"], json!("element"));
        assert_eq!(data["candidates"], json!(["textLabel"]), "{data:?}");
    }

    #[test]
    fn a_misspelt_attribute_carries_its_candidates() {
        let analysis = analyse("local create = f()\nlocal e = <Frame Color3={c}/>\n");
        let data = &analysis.diagnostics[0]["data"];
        assert_eq!(data["kind"], json!("member"));

        let candidates = data["candidates"].as_array().expect("candidates");
        assert!(candidates.contains(&json!("BackgroundColor3")), "{candidates:?}");
    }

    /// Option+Space on a Mac types `\u{a0}`, which the LuauX parser does not
    /// count as whitespace — so it fails at that byte with a zero-length error,
    /// and widening that by one byte used to land mid-character and panic.
    #[test]
    fn a_non_breaking_space_does_not_bring_the_server_down() {
        let analysis = analyse("local create = f()\nlocal e = <TextButton\u{a0}Text='x'/>\n");
        assert!(!analysis.diagnostics.is_empty());

        // The range covers the whole character, and is expressible in UTF-16.
        let range = &analysis.diagnostics[0]["range"];
        assert_eq!(range["start"]["line"], json!(1));
        assert!(range["end"]["character"].as_u64() > range["start"]["character"].as_u64());
    }

    #[test]
    fn every_byte_of_a_multi_byte_file_is_a_safe_diagnostic_offset() {
        // The compiler reports offsets in bytes and this has to survive all of
        // them, not just the ones a fixture happens to produce.
        let text = "local e = <Frame Name='ü\u{a0}😀'/>\n";
        let document = Document::new("file:///a.luaux".into(), 1, text.into());
        let project = Project::discover(Path::new("/nonexistent-luaux-project/a.luaux"));

        for offset in 0..=text.len() + 4 {
            let _ = diagnostic(&document, &project, "synthetic", offset, 0, None, 1);
            let _ = diagnostic(&document, &project, "synthetic", offset, 3, None, 1);
        }
    }

    #[test]
    fn a_warning_is_a_warning() {
        let analysis = analyse(
            "local create = f()\nlocal e = <Frame>{cond() and <TextLabel/> or nil}</Frame>\n",
        );
        assert_eq!(analysis.diagnostics.len(), 1, "{:?}", messages(&analysis));
        assert_eq!(analysis.diagnostics[0]["severity"], json!(2));
    }

    /// A resolution error no longer costs the file: the compiler recovers, so
    /// there is fresh output and the diagnostic sits beside it. Only a *parse*
    /// error leaves nothing to map, and that is what the stale fallback is for.
    #[test]
    fn an_unknown_element_still_compiles_and_still_reports() {
        let analysis = analyse("local create = f()\nlocal e = <Frmae/>\n");

        assert_eq!(analysis.diagnostics.len(), 1, "{:?}", messages(&analysis));
        assert_eq!(analysis.diagnostics[0]["severity"], json!(1));
        // Fresh, not the previous compile: there was no previous compile.
        assert!(analysis.compiled.is_some());
        assert!(!analysis.stale);
    }

    #[test]
    fn every_unknown_element_is_reported_not_just_the_first() {
        let analysis = analyse("local create = f()\nlocal e = <Frmae><Recieve/><Buton/></Frmae>\n");

        let messages = messages(&analysis);
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert!(messages[0].contains("Frmae"), "{messages:?}");
        assert!(messages[2].contains("Buton"), "{messages:?}");
    }

    #[test]
    fn a_broken_file_keeps_the_last_good_compile_and_marks_it_stale() {
        let good = analyse("local create = f()\nlocal e = <Frame/>\n");
        let previous = good.compiled.expect("compiled");

        // A *parse* error, which is the kind that still leaves nothing to map.
        // Same line count, so the map's line correspondence still holds.
        let document = Document::new(
            "file:///a.luaux".into(),
            2,
            "local create = f()\nlocal e = <Frame\n".into(),
        );
        let project = Project::discover(Path::new("/nonexistent-luaux-project/a.luaux"));
        let analysis = Analysis::run(&document, &project, Some(previous));

        assert!(analysis.stale);
        assert!(analysis.compiled.is_some());
        assert_eq!(analysis.diagnostics.len(), 1);
    }

    #[test]
    fn the_fallback_is_dropped_once_lines_move() {
        let good = analyse("local create = f()\nlocal e = <Frame/>\n");
        let previous = good.compiled.expect("compiled");

        // A new line makes every line number in the old map a lie.
        let document = Document::new(
            "file:///a.luaux".into(),
            2,
            "local create = f()\n\nlocal e = <Frame\n".into(),
        );
        let project = Project::discover(Path::new("/nonexistent-luaux-project/a.luaux"));
        let analysis = Analysis::run(&document, &project, Some(previous));

        assert!(!analysis.stale);
        assert!(analysis.compiled.is_none());
    }
}
