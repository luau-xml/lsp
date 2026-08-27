//! luaux-lsp — the LuauX language server.
//!
//! It owns `.luaux`, answers every question that is about the markup, and
//! forwards everything that is about Luau to a stock `luau-lsp` with positions
//! translated in both directions.

pub mod analysis;
pub mod api;
pub mod bindings;
pub mod code_actions;
pub mod completion;
pub mod document;
pub mod hover;
pub mod jsonrpc;
pub mod line_index;
pub mod map_builder;
pub mod naming;
pub mod project;
pub mod proxy;
pub mod regions;
pub mod remap;
pub mod rename;
pub mod scan;
pub mod semantic_tokens;
pub mod server;
pub mod sourcemap;
pub mod symbols;
pub mod tree;
pub mod workspace;

/// The code generator a project's `[factory] backend` selects.
///
/// luaux 0.2.0 replaced the single Vide backend with two *arrangements* — which
/// is the only thing a backend now decides — and the choice is the project's,
/// not ours:
///
/// * [`Table`] — `F(class)(props)`, children in the props table. Vide, Fusion,
///   Fluid.
/// * [`Element`] — `F(class, props, children)`, children positional. React.
///
/// Every compile in this server goes through here, because compiling with the
/// wrong arrangement is not a near miss: the generated call has a different
/// shape, so the source map's anchors miss, and every forwarded position in the
/// file is then either absent or — worse — pointing somewhere the author did
/// not write.
pub fn backend(config: &luaux::Config) -> &'static dyn luaux::Backend {
    match config.backend {
        luaux::config::BackendKind::Table => &luaux::backend::Table,
        luaux::config::BackendKind::Element => &luaux::backend::Element,
    }
}

/// Version of this server.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version of the compiler it was built against.
///
/// Reported on startup and in `initialize`, because a server built against a
/// different compiler than the one producing `build/` reports diagnostics the
/// build does not — and that mismatch is otherwise invisible.
pub const LUAUX_VERSION: &str = env!("LUAUX_VERSION");
