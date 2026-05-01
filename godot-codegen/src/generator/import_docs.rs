/*
 * Copyright (c) godot-rust; Bromeon and contributors.
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::fmt::Write as _;

use crate::context::Context;
use crate::models::domain::{ApiView, Class, ClassLike, Function, TyName};
use crate::{conv, special_cases, util};

type CowStr = std::borrow::Cow<'static, str>;

/// Infallible `write!`.
macro_rules! write_str {
    ($out:expr, $($arg:tt)*) => {
        write!($out, $($arg)*).expect("writing to String should not fail")
    };
}

pub fn import_docs(
    description: &str,
    surrounding_class: Option<&Class>,
    ctx: &Context,
    view: &ApiView,
) -> String {
    DocImporter::new(description, surrounding_class, ctx, view).import()
}

pub fn import_function_docs(fun: &dyn Function, ctx: &Context, view: &ApiView) -> Option<String> {
    let doc = fun.common().description.as_ref()?;
    if doc.is_empty() {
        return None;
    }
    let surrounding_class_name = fun.surrounding_class();
    let surrounding_class = surrounding_class_name.and_then(|name| view.find_engine_class(name));
    let imported_doc = import_docs(doc, surrounding_class, ctx, view);
    // let imported_doc = format!("\n# Godot docs\n{imported_doc}"); -- no title at the moment.
    Some(imported_doc)
}

fn matches_primitive_type(ty: &str) -> bool {
    matches!(ty, "int" | "float" | "bool")
}

fn matches_ignored_links(class: &str) -> bool {
    // We don't have a single place to point @GDScript to.
    class == "@GDScript"
}

/// Flags controlling how a parse region is rendered.
#[derive(Copy, Clone)]
struct ParseMode {
    /// Convert single `\n` in source to `\n\n` (Markdown paragraph break).
    double_newlines: bool,
    /// Turn `[Type]` brackets into Rustdoc links; off inside code regions.
    allow_type_links: bool,
    /// Recurse into BBCode tags; off inside code regions where content is literal.
    allow_tags: bool,
}

impl ParseMode {
    /// Top-level prose: paragraph breaks, type links, BBCode all enabled.
    const TOP: Self = Self {
        double_newlines: true,
        allow_type_links: true,
        allow_tags: true,
    };
    /// Inside fenced/inline code: keep content literal, no paragraph breaks, no recursion.
    const CODE: Self = Self {
        double_newlines: false,
        allow_type_links: false,
        allow_tags: false,
    };

    /// Derive inner-content mode for a wrapped tag.
    ///
    /// Formatting tags (`[b]`, `[i]`, `[kbd]`) inherit the outer mode so nested links/tags work.
    /// Code tags switch to [`Self::CODE`] so brackets inside code stay literal.
    fn inherit_for(self, tag: &WrappedTag) -> Self {
        if tag.allow_inner {
            Self {
                double_newlines: self.double_newlines,
                allow_type_links: self.allow_type_links,
                allow_tags: true,
            }
        } else {
            Self::CODE
        }
    }
}

/// BBCode tag with a fixed opener/closer and a literal Markdown wrapping.
///
/// Used for tags whose opener has no dynamic attribute. Tags with attributes
/// (`[url=...]`, `[codeblock lang=...]`) are handled by separate parsers.
struct WrappedTag {
    /// BBCode opener, e.g. `"[b]"`.
    open: &'static str,
    /// BBCode closer, e.g. `"[/b]"`.
    close: &'static str,
    /// Markdown to emit before inner content, e.g. `"**"` or `"```gdscript"`.
    prefix: &'static str,
    /// Markdown to emit after inner content, e.g. `"**"` or `"```"`.
    suffix: &'static str,
    /// If true, inner content is parsed with the outer mode (formatting tags like `[b]`, `[i]`, `[kbd]`).
    /// If false, inner content is parsed in [`ParseMode::CODE`] (fenced/inline code blocks).
    allow_inner: bool,
}

// Order matters: longer prefix first when prefixes overlap (`[code skip-lint]` before `[code]`).
#[rustfmt::skip]
const WRAPPED_TAGS: &[WrappedTag] = &[
    WrappedTag { open: "[b]",              close: "[/b]",         prefix: "**",            suffix: "**",  allow_inner: true  },
    WrappedTag { open: "[i]",              close: "[/i]",         prefix: "_",             suffix: "_",   allow_inner: true  },
    WrappedTag { open: "[kbd]",            close: "[/kbd]",       prefix: "`",             suffix: "`",   allow_inner: true  },
    WrappedTag { open: "[code skip-lint]", close: "[/code]",      prefix: "`",             suffix: "`",   allow_inner: false },
    WrappedTag { open: "[code]",           close: "[/code]",      prefix: "`",             suffix: "`",   allow_inner: false },
    WrappedTag { open: "[codeblock]",      close: "[/codeblock]", prefix: "```gdscript",   suffix: "```", allow_inner: false },
    WrappedTag { open: "[gdscript]",       close: "[/gdscript]",  prefix: "```gdscript",   suffix: "```", allow_inner: false },
    WrappedTag { open: "[csharp]",         close: "[/csharp]",    prefix: "```csharp",     suffix: "```", allow_inner: false },
];

struct DocImporter<'d> {
    doc: &'d str,
    pos: usize,
    surrounding_class: Option<&'d Class>,
    ctx: &'d Context<'d>,
    view: &'d ApiView<'d>,
}

impl<'d> DocImporter<'d> {
    fn new(
        doc: &'d str,
        surrounding_class: Option<&'d Class>,
        ctx: &'d Context<'d>,
        view: &'d ApiView<'d>,
    ) -> Self {
        Self {
            doc,
            pos: 0,
            surrounding_class,
            ctx,
            view,
        }
    }

    fn import(mut self) -> String {
        // Output grows ~3-4x when type links expand to `[`Foo`][crate::classes::Foo]`; reserve up front.
        let mut out = String::with_capacity(self.doc.len() * 4);
        let ok = self.parse_until(&mut out, None, ParseMode::TOP);
        debug_assert!(ok, "top-level parse_until without closing tag must succeed");
        out
    }

    /// Snapshot of `(self.pos, out.len())` for transactional rollback on failed sub-parses.
    fn checkpoint(&self, out: &str) -> (usize, usize) {
        (self.pos, out.len())
    }

    /// Restore both input position and output length from a [`Self::checkpoint`].
    /// Used when a sub-parser starts emitting then fails (e.g. unterminated tag) and must
    /// leave the source byte-for-byte for the fallback parser to consume.
    fn rollback(&mut self, out: &mut String, cp: (usize, usize)) {
        self.pos = cp.0;
        out.truncate(cp.1);
    }

    // Parses the doc, writing rendered output into `out`.
    // - If `closing_tag` is given, returns true when found and consumed.
    // - On EOF without closing tag, rolls back `out` and `self.pos` and returns false.
    // - With `closing_tag = None`, always succeeds at EOF.
    fn parse_until(
        &mut self,
        out: &mut String,
        closing_tag: Option<&str>,
        mode: ParseMode,
    ) -> bool {
        let cp = self.checkpoint(out);

        while self.pos < self.doc.len() {
            if let Some(close) = closing_tag
                && self.remaining().starts_with(close)
            {
                self.pos += close.len();
                return true;
            }

            if mode.allow_tags && self.remaining().starts_with('[') && self.try_parse_tag(out, mode)
            {
                continue;
            }

            // unwrap(): loop guard ensures self.pos < self.doc.len(), so a char is present.
            let ch = self.remaining().chars().next().unwrap();
            self.pos += ch.len_utf8();
            if ch == '\n' && mode.double_newlines {
                out.push_str("\n\n");
            } else {
                out.push(ch);
            }
        }

        if closing_tag.is_none() {
            true
        } else {
            self.rollback(out, cp);
            false
        }
    }

    /// Dispatch a `[...]` opener to the matching parser.
    ///
    /// Tries parsers in order:
    /// 1. Static [`WRAPPED_TAGS`] table.
    /// 2. Attribute-bearing tags: `[url=...]`, `[codeblocks]`, `[codeblock lang=...]`.
    /// 3. If a known BBCode opener is unterminated, return `false` -> the caller emits the bracket literally.
    /// 4. Generic Markdown link or type-link role.
    fn try_parse_tag(&mut self, out: &mut String, mode: ParseMode) -> bool {
        // Try the static table of wrapped tags first. An unterminated real opener is emitted as-is,
        // not reinterpreted by the bracket-link parser below.
        for tag in WRAPPED_TAGS {
            if self.remaining().starts_with(tag.open) && self.try_wrapped_tag(out, tag, mode) {
                return true;
            }
        }

        if self.try_url_tag(out, mode)
            || self.try_codeblocks_tag(out)
            || self.try_codeblock_lang_tag(out)
        {
            return true;
        }

        if starts_with_known_tag(self.remaining()) {
            return false;
        }

        self.try_markdown_link(out) || self.try_bracket_link(out, mode.allow_type_links)
    }

    fn try_wrapped_tag(&mut self, out: &mut String, tag: &WrappedTag, mode: ParseMode) -> bool {
        if !self.remaining().starts_with(tag.open) {
            return false;
        }

        let cp = self.checkpoint(out);
        self.pos += tag.open.len();
        out.push_str(tag.prefix);
        if !self.parse_until(out, Some(tag.close), mode.inherit_for(tag)) {
            self.rollback(out, cp);
            return false;
        }
        out.push_str(tag.suffix);
        true
    }

    // Consume an opener of the form `<prefix>VALUE]`. Advances `self.pos` past the `]`.
    // Returns the slice between `<prefix>` and `]` (the attribute value), or None if no match.
    //
    // Shortcoming: searches for the first `]` in the remaining input, so an attribute value containing `]` (e.g. `[url=https://x/a]b]`)
    // would truncate. Godot's docs should not produce such values in practice, so this is accepted.
    fn try_consume_attr_opener(&mut self, prefix: &str) -> Option<&'d str> {
        let remaining = self.remaining();
        if !remaining.starts_with(prefix) {
            return None;
        }
        let end = remaining.find(']')?;
        let value = &remaining[prefix.len()..end];
        self.pos += end + 1;
        Some(value)
    }

    fn try_url_tag(&mut self, out: &mut String, mode: ParseMode) -> bool {
        const PREFIX: &str = "[url=";
        const SUFFIX: &str = "[/url]";

        let cp = self.checkpoint(out);
        let Some(url) = self.try_consume_attr_opener(PREFIX) else {
            return false;
        };
        let url = url.to_owned(); // detach borrow from `self` before recursing.

        out.push('[');
        if !self.parse_until(out, Some(SUFFIX), mode) {
            self.rollback(out, cp);
            return false;
        }
        write_str!(out, "]({url})");
        true
    }

    fn try_codeblocks_tag(&mut self, out: &mut String) -> bool {
        const OPENING_TAG: &str = "[codeblocks]";
        const CLOSING_TAG: &str = "[/codeblocks]";

        if !self.remaining().starts_with(OPENING_TAG) {
            return false;
        }

        let cp = self.checkpoint(out);
        self.pos += OPENING_TAG.len();
        // `[codeblocks]` is a container for nested language blocks, not a literal fence itself.
        let inner = ParseMode {
            double_newlines: false,
            allow_type_links: true,
            allow_tags: true,
        };
        if !self.parse_until(out, Some(CLOSING_TAG), inner) {
            self.rollback(out, cp);
            return false;
        }
        true
    }

    fn try_codeblock_lang_tag(&mut self, out: &mut String) -> bool {
        const PREFIX: &str = "[codeblock lang=";
        const SUFFIX: &str = "[/codeblock]";

        let cp = self.checkpoint(out);
        let Some(lang) = self.try_consume_attr_opener(PREFIX) else {
            return false;
        };
        let lang = lang.to_owned(); // detach borrow from `self` before recursing.

        write_str!(out, "```{lang}");
        // The body is literal fenced code, so bracket roles should not be interpreted inside it.
        if !self.parse_until(out, Some(SUFFIX), ParseMode::CODE) {
            self.rollback(out, cp);
            return false;
        }
        out.push_str("```");
        true
    }

    /// Pass-through for inline Markdown links `[text](http(s)://...)` already present in source.
    /// Bare `[Type](suffix)` (no `http`) is left for [`Self::try_bracket_link`] to handle.
    ///
    /// Shortcoming: scans for the first `]` and `)`, so nested brackets in link text (e.g. `[a [Node] b](http://x)`) would mis-parse.
    /// Not produced by Godot in practice.
    fn try_markdown_link(&mut self, out: &mut String) -> bool {
        let remaining = self.remaining();
        if !remaining.starts_with('[') {
            return false;
        }

        let Some(end_of_text) = remaining.find(']') else {
            return false;
        };
        let after_text = &remaining[end_of_text + 1..];
        // Preserve real inline Markdown links, but let `[Type](s)` fall back to type-link parsing.
        if !after_text.starts_with("(http") {
            return false;
        }

        let Some(end_of_target) = after_text.find(')') else {
            return false;
        };
        let len = end_of_text + 1 + end_of_target + 1;
        out.push_str(&remaining[..len]);
        self.pos += len;
        true
    }

    /// Handle role-prefixed brackets (`[param X]`, `[method Class.fn]`, `[signal X]`,
    /// `[annotation X]`, `[constructor Type]`), bare type links (`[Node]`), and
    /// "escaped" roles whose target we cannot resolve (`[member X.y]`, `[constant X]`, ...).
    /// Unrecognized brackets return `false` so the caller emits them literally.
    fn try_bracket_link(&mut self, out: &mut String, allow_type_links: bool) -> bool {
        let remaining = self.remaining();
        if !remaining.starts_with('[') {
            return false;
        }

        let Some(end) = remaining.find(']') else {
            return false;
        };
        let whole = &remaining[..=end];
        let content = &remaining[1..end];

        if let Some(param_name) = content.strip_prefix("param ")
            && is_ident_like(param_name)
        {
            self.pos += whole.len();
            write_str!(out, "`{param_name}`");
            return true;
        }

        if let Some(method_path) = content.strip_prefix("method ") {
            self.pos += whole.len();
            self.write_method_link(out, whole, method_path);
            return true;
        }

        if let Some(signal_name) = content.strip_prefix("signal ") {
            self.pos += whole.len();
            write_code_span(out, signal_name);
            return true;
        }

        if let Some(annotation) = content.strip_prefix("annotation ") {
            self.pos += whole.len();
            write_code_span(out, annotation);
            return true;
        }

        if let Some(constant) = content.strip_prefix("constant ") {
            self.pos += whole.len();
            write_code_span(out, constant);
            return true;
        }

        if let Some(ty_name) = content.strip_prefix("constructor ") {
            self.pos += whole.len();
            out.push('`');
            out.push_str(ty_name);
            out.push_str("()`");
            return true;
        }

        if is_escaped_role(content) {
            self.pos += whole.len();
            write_str!(out, "\\{whole}");
            return true;
        }

        if allow_type_links && is_type_link(content) {
            self.pos += whole.len();
            self.write_type_link(out, content);
            return true;
        }

        false
    }

    /// Emit Markdown for `[Foo]`. Branches:
    /// - deleted/`@GDScript` → plain text;
    /// - `int`/`float`/`bool` → `` `int` `` (no link target);
    /// - hardcoded specials (`@GlobalScope`) → fixed link;
    /// - link to surrounding class → `` `Self` ``-style code span (avoid self-link);
    /// - else → `` [`Foo`][crate::classes::Foo] `` Rustdoc reference link.
    fn write_type_link(&self, out: &mut String, ty_name: &str) {
        if special_cases::is_godot_type_deleted(ty_name) || matches_ignored_links(ty_name) {
            out.push_str(ty_name);
        } else if matches_primitive_type(ty_name) {
            write_code_span(out, ty_name);
        } else if let Some(hardcoded) = matches_hardcoded_type(ty_name) {
            out.push_str(hardcoded);
        } else {
            // Compare on the Godot name to skip the per-link `to_pascal_case` allocation.
            let is_link_to_surrounding_class = self
                .surrounding_class
                .is_some_and(|c| c.name().godot_ty == ty_name);

            if is_link_to_surrounding_class {
                write_code_span(out, ty_name);
            } else {
                let path = get_class_rust_path(ty_name, self.ctx);
                write_code_link(out, ty_name, &path);
            }
        }
    }

    /// Emit Markdown for `[method Class.fn]` or `[method fn]` (latter resolved against surrounding class).
    /// Falls back to escaping the original `[method ...]` literal if the target cannot be resolved.
    fn write_method_link(&self, out: &mut String, whole_match: &str, method_path: &str) {
        if let Some(method_path) =
            convert_to_method_path(method_path, self.surrounding_class, self.ctx, self.view)
        {
            let (_, method_name) = method_path
                .rsplit_once("::")
                .expect("rsplit_once should return a method name");
            write_str!(out, "[`{method_name}`][`{method_path}`]");
        } else {
            write_str!(out, "\\{whole_match}");
        }
    }

    fn remaining(&self) -> &'d str {
        &self.doc[self.pos..]
    }
}

/// Emit `` `text` ``.
fn write_code_span(out: &mut String, text: &str) {
    out.push('`');
    out.push_str(text);
    out.push('`');
}

/// Emit a Rustdoc reference link `` [`label`][path] ``.
fn write_code_link(out: &mut String, label: &str, path: &str) {
    out.push('[');
    out.push('`');
    out.push_str(label);
    out.push('`');
    out.push_str("][");
    out.push_str(path);
    out.push(']');
}

/// Bracketed name acceptable as a parameter/identifier (ASCII alphanumeric + underscore).
fn is_ident_like(str: &str) -> bool {
    !str.is_empty()
        && str
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Bracketed name shaped like a Godot class link: ASCII alphanumeric, optionally prefixed with `@`
/// for global namespaces (`@GlobalScope`, `@GDScript`). `@` only allowed at position 0.
fn is_type_link(str: &str) -> bool {
    let mut chars = str.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() && first != '@' {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric())
}

/// True if `str` begins with a recognized BBCode opener. Used by [`DocImporter::try_parse_tag`]
/// to emit unterminated known openers as-is, instead of letting the bracket-link parser reinterpret them.
fn starts_with_known_tag(str: &str) -> bool {
    WRAPPED_TAGS.iter().any(|t| str.starts_with(t.open))
        || str.starts_with("[url=")
        || str.starts_with("[codeblocks]")
        || str.starts_with("[codeblock lang=")
}

/// Roles we recognize but cannot resolve to a Rust target — emit them as escaped literal `\[role X]`
/// so Markdown does not interpret them as links. Accepts both `[role arg]` and bare `[role]`.
fn is_escaped_role(str: &str) -> bool {
    let role = str.split_once(' ').map(|(r, _)| r).unwrap_or(str);
    matches!(
        role,
        "constant" | "member" | "enum" | "signal" | "annotation" | "constructor"
    )
}

fn convert_to_method_path(
    class_method: &str,
    surrounding_class: Option<&Class>,
    ctx: &Context,
    view: &ApiView,
) -> Option<CowStr> {
    // Get the class name from the link if it has one, otherwise, use the surrounding class's name.
    // For example, if we are generating docs for the `CanvasItem` class and see an "Object._notification" link
    // take "Object" as the class name and "_notification" as the method name. But if we see a "queue_redraw"
    // link, take the surrounding class's name(in our case it's "CanvasItem") as the class that owns the method.
    let (link_godot_class, link_godot_method) =
        if let Some((class_name, method_name)) = class_method.split_once('.') {
            (class_name, method_name)
        } else if let Some(class) = surrounding_class {
            (class.name().godot_ty.as_str(), class_method)
        } else {
            return None;
        };

    if special_cases::is_godot_type_deleted(link_godot_class) {
        return None;
    }

    let link_godot_method = util::safe_ident(link_godot_method).to_string();

    // These cover renamed helpers and special symbols that do not map 1:1 through the API view.
    match matches_hardcoded_method(link_godot_class, &link_godot_method) {
        Hardcoded::Mapped(path) => return Some(path),
        Hardcoded::Suppressed => return None,
        Hardcoded::NotMatched => {}
    }

    if let Some(class) = view.find_engine_class(&TyName::from_godot(link_godot_class))
        && let Some(method) = class
            .methods
            .iter()
            .find(|method| method.godot_name() == link_godot_method)
    {
        let rust_method_name = method.name();

        // Skip links to private methods.
        if method.is_private_in_final_api() {
            return None;
        }
        if method.is_virtual() {
            return if class.is_final {
                // Final classes don't have an associated trait with virtual methods.
                None
            } else {
                let path = format!(
                    "crate::classes::{}::{}",
                    class.name().virtual_trait_name(),
                    rust_method_name
                );
                Some(path.into())
            };
        }

        // Use the Rust name; covers `special_cases::maybe_rename_class_method`.
        // Examples: `Object.get_script` -> `raw_get_script`, `GDScript.new` -> `instantiate`.
        let rust_class_path = get_class_rust_path(link_godot_class, ctx);
        return Some(format!("{rust_class_path}::{rust_method_name}").into());
    }

    // Fallback when class/method is not in the API view (e.g. unknown class link): strip the leading underscore
    // as a best-effort guess at the Rust name.
    let godot_method_name = link_godot_method.trim_start_matches("_");
    let rust_class_path = get_class_rust_path(link_godot_class, ctx);
    Some(format!("{rust_class_path}::{godot_method_name}").into())
}

fn matches_hardcoded_type(godot_class: &str) -> Option<&'static str> {
    match godot_class {
        "@GlobalScope" => Some("[@GlobalScope][crate::global]"),
        _ => None,
    }
}

enum Hardcoded {
    /// Matched a special-cased mapping; use this Rust path.
    Mapped(CowStr),
    /// Matched, but link should be dropped (no Rust target — e.g. arbitrary `@GDScript` symbols).
    Suppressed,
    /// No special case; fall through to the regular API-view lookup.
    NotMatched,
}

fn matches_hardcoded_method(godot_class: &str, godot_method: &str) -> Hardcoded {
    let path: CowStr = match (godot_class, godot_method) {
        ("Object", "free") => "crate::obj::Gd::free".into(),
        ("Object", "get_instance_id") => "crate::obj::Gd::instance_id".into(),
        ("Object", "notification") => "crate::classes::Object::notify".into(),
        ("Object", "_notification") => "crate::classes::IObject::on_notification".into(),
        ("Object", "_init") => "crate::classes::IObject::init".into(),
        ("Object", "_validate_property") => "crate::classes::IObject::on_validate_property".into(),
        ("Object", "_get_property_list") => "crate::classes::IObject::on_get_property_list".into(),
        ("Object", "_get") => "crate::classes::IObject::on_get".into(),
        ("Object", "_set") => "crate::classes::IObject::on_set".into(),
        ("GDScript", "new") => "crate::obj::NewGd::new_gd".into(),
        ("String", "length") => "crate::builtin::GString::len".into(),
        ("String", "match_") => "crate::builtin::GString::match_glob".into(),
        ("Dictionary", "size") => "crate::builtin::Dictionary::len".into(),
        ("Array", "size") => "crate::builtin::AnyArray::len".into(),
        ("PackedByteArray", "size") => "crate::builtin::PackedByteArray::len".into(),
        ("Vector2", "min") => "crate::builtin::Vector2::coord_min".into(),
        ("Vector2", "max") => "crate::builtin::Vector2::coord_max".into(),
        ("Vector3", "min") => "crate::builtin::Vector3::coord_min".into(),
        ("Vector3", "max") => "crate::builtin::Vector3::coord_max".into(),
        ("Vector4", "min") => "crate::builtin::Vector4::coord_min".into(),
        ("Vector4", "max") => "crate::builtin::Vector4::coord_max".into(),
        ("Transform2D", "get_scale") => "crate::builtin::Transform2D::scale".into(),
        ("Node", "get_node") => "crate::classes::Node::get_node_as".into(),
        ("Color", "is_equal_approx") => "crate::builtin::math::ApproxEq::approx_eq".into(),
        ("@GlobalScope", "instance_from_id") => "crate::obj::Gd::from_instance_id".into(),
        ("@GlobalScope", "is_instance_valid") => "crate::obj::Gd::is_instance_valid".into(),
        ("@GDScript", "load") => "crate::tools::load".into(),
        ("@GDScript", "save") => "crate::tools::save".into(),
        ("@GlobalScope", _) => format!("crate::global::{godot_method}").into(),
        ("@GDScript", _) => return Hardcoded::Suppressed,
        _ => return Hardcoded::NotMatched,
    };

    Hardcoded::Mapped(path)
}

fn convert_builtin_types(type_name: &str) -> Option<&'static str> {
    match type_name {
        "String" => Some("crate::builtin::GString"),
        "Array" => Some("crate::builtin::Array"),
        "Dictionary" => Some("crate::builtin::Dictionary"),
        _ => None,
    }
}

// Optimization: results could be memoized via `HashMap<&str, CowStr>` on `Context` to avoid re-formatting the same `crate::classes::MyClass`
// path per type link. Only worthwhile if doc import shows up in codegen profiles.
fn get_class_rust_path(godot_class_name: &str, ctx: &Context) -> CowStr {
    if let Some(hardcoded_builtin_type) = convert_builtin_types(godot_class_name) {
        return hardcoded_builtin_type.into();
    }

    let is_builtin = ctx.is_builtin(godot_class_name);
    let rust_class_name = conv::to_pascal_case(godot_class_name);
    if is_builtin {
        format!("crate::builtin::{rust_class_name}").into()
    } else {
        format!("crate::classes::{rust_class_name}").into()
    }
}

// ----------------------------------------------------------------------------------------------------------------------------------------------
// Unit tests

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use std::cell::OnceCell;

    use super::*;
    use crate::models::api_json::{JsonExtensionApi, load_extension_api};
    use crate::models::domain::ExtensionApi;

    // `JsonExtensionApi` and `ExtensionApi` are cached per thread; `Context`/`ApiView` are
    // cheap to rebuild and need lifetimes tied to those owners, so we rebuild them per test.
    // Using `thread_local` (rather than a global `OnceLock`) avoids needing a `Mutex`, since
    // `ExtensionApi` contains `proc_macro2::TokenStream` and is therefore `!Sync`.
    struct DocTestCache {
        json: JsonExtensionApi,
        api: ExtensionApi,
    }

    thread_local! {
        static CACHE: OnceCell<DocTestCache> = const { OnceCell::new() };
    }

    fn import_doc_for_test(description: &str, surrounding_class_name: Option<&str>) -> String {
        CACHE.with(|cell| {
            let cache = cell.get_or_init(|| {
                let mut watch = godot_bindings::StopWatch::start();
                let json = load_extension_api(&mut watch);
                let mut ctx = Context::build_from_api(&json);
                let api = ExtensionApi::from_json(&json, &mut ctx);
                DocTestCache { json, api }
            });

            let ctx = Context::build_from_api(&cache.json);
            let view = ApiView::new(&cache.api);
            let surrounding_class = surrounding_class_name
                .and_then(|name| view.find_engine_class(&TyName::from_godot(name)));

            import_docs(description, surrounding_class, &ctx, &view)
        })
    }

    // Bare Godot type links become Rustdoc links with code-formatted labels.
    #[test]
    fn type__engine_classes() {
        let description = "Left side, usually used for [Control] or [StyleBox]-derived classes.";

        let actual = import_doc_for_test(description, None);

        assert_eq!(
            actual,
            "Left side, usually used for [`Control`][crate::classes::Control] or [`StyleBox`][crate::classes::StyleBox]-derived classes."
        );
    }

    #[test]
    fn type__builtin_and_member_role() {
        let description = "Link [member Vector2.x] and [member Vector2.y] on [Vector2] or \
            [Vector3]. Use [code]\"suffix:px/s\"[/code] for the editor unit suffix.";

        let actual = import_doc_for_test(description, None);

        assert_eq!(
            actual,
            "Link \\[member Vector2.x] and \\[member Vector2.y] on \
            [`Vector2`][crate::builtin::Vector2] or [`Vector3`][crate::builtin::Vector3]. Use \
            `\"suffix:px/s\"` for the editor unit suffix."
        );
    }

    // Existing Markdown links must stay untouched while later bare type links are still imported.
    #[test]
    fn type__preserves_markdown_link() {
        let description = "See [reference](https://example.com) and [Node].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(
            actual,
            "See [reference](https://example.com) and [`Node`][crate::classes::Node]."
        );
    }

    #[test]
    fn type__global_scope() {
        let description = "Use [@GlobalScope].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "Use [@GlobalScope][crate::global].");
    }

    // Bare `@GDScript` stays plain until there is a dedicated Rust target for it.
    #[test]
    fn type__gdscript_plain_text() {
        let description = "See [@GDScript].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "See @GDScript.");
    }

    #[test]
    fn type__primitive_links() {
        let description = "Use [int], [float], and [bool].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "Use `int`, `float`, and `bool`.");
    }

    // Links to the current class are rendered as code to avoid redundant Markdown links.
    #[test]
    fn type__surrounding_class() {
        let description = "See [Node] and [Object].";

        let actual = import_doc_for_test(description, Some("Node"));

        assert_eq!(actual, "See `Node` and [`Object`][crate::classes::Object].");
    }

    #[test]
    fn method__with_newlines_and_roles() {
        let description = "Compare [code]LEFT[/code] and [code]RIGHT[/code] variants.\n\
            Use [method InputEvent.is_match] with [constant KEY_LOCATION_UNSPECIFIED] or [enum KeyLocation].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(
            actual,
            "Compare `LEFT` and `RIGHT` variants.\n\n\
            Use [`is_match`][`crate::classes::InputEvent::is_match`] with `KEY_LOCATION_UNSPECIFIED` or \\[enum KeyLocation]."
        );
    }

    #[test]
    fn method__in_surrounding_class() {
        let description = "Call [method get_node] to fetch a child.";

        let actual = import_doc_for_test(description, Some("Node"));

        assert_eq!(
            actual,
            "Call [`get_node_as`][`crate::classes::Node::get_node_as`] to fetch a child."
        );
    }

    // Regression: methods renamed via `special_cases::maybe_rename_class_method` must use the Rust name in the link.
    // Here type-safe replacement: `Object.get_script` -> `raw_get_script`.
    #[test]
    fn method__renamed_via_special_case() {
        let description = "See [method Object.get_script].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(
            actual,
            "See [`raw_get_script`][`crate::classes::Object::raw_get_script`]."
        );
    }

    // `[constant X]` is rendered as code, since we have no Rust target for arbitrary constants.
    #[test]
    fn constant__code_fallback() {
        let description = "See [constant NOTIFICATION_ENTER_TREE].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "See `NOTIFICATION_ENTER_TREE`.");
    }

    #[test]
    fn code_block__preserves_contents() {
        let description = "Bit mask used to remove modifiers before checking a keycode.\n\
            [codeblock]\n\
            var keycode = KEY_A | KEY_MASK_SHIFT\n\
            keycode = keycode & KEY_CODE_MASK\n\
            [/codeblock]";

        let actual = import_doc_for_test(description, None);

        assert_eq!(
            actual,
            "Bit mask used to remove modifiers before checking a keycode.\n\n\
            ```gdscript\n\
            var keycode = KEY_A | KEY_MASK_SHIFT\n\
            keycode = keycode & KEY_CODE_MASK\n\
            ```"
        );
    }

    // Fenced code blocks keep bracketed type-like text literal.
    #[test]
    fn code_block__type_literal() {
        let description = "[codeblock]\n[Node]\n[/codeblock]";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "```gdscript\n[Node]\n```");
    }

    // Covers [codeblocks], [codeblock lang=...], [gdscript], and [csharp].
    #[test]
    fn code_block__other_variants() {
        let codeblocks_description = "[codeblocks]alpha\nbeta[/codeblocks]";
        let codeblock_lang_description = "[codeblock lang=text]\nalpha\nbeta\n[/codeblock]";
        let gdscript_description = "[gdscript]\nprint(\"hi\")\n[/gdscript]";
        let csharp_description = "[csharp]\nGD.Print(\"hi\");\n[/csharp]";

        let codeblocks_actual = import_doc_for_test(codeblocks_description, None);
        let codeblock_lang_actual = import_doc_for_test(codeblock_lang_description, None);
        let gdscript_actual = import_doc_for_test(gdscript_description, None);
        let csharp_actual = import_doc_for_test(csharp_description, None);

        assert_eq!(codeblocks_actual, "alpha\nbeta");
        assert_eq!(codeblock_lang_actual, "```text\nalpha\nbeta\n```");
        assert_eq!(gdscript_actual, "```gdscript\nprint(\"hi\")\n```");
        assert_eq!(csharp_actual, "```csharp\nGD.Print(\"hi\");\n```");
    }

    #[test]
    fn codeblocks__nested_languages() {
        let description = "[codeblocks]\n\
            [gdscript]\n\
            print(\"hi\")\n\
            [/gdscript]\n\
            [csharp]\n\
            GD.Print(\"hi\");\n\
            [/csharp]\n\
            [/codeblocks]";

        let actual = import_doc_for_test(description, None);

        assert_eq!(
            actual,
            "\n\
            ```gdscript\n\
            print(\"hi\")\n\
            ```\n\
            ```csharp\n\
            GD.Print(\"hi\");\n\
            ```\n"
        );
    }

    #[test]
    fn code__skip_lint() {
        let description =
            "Use [code skip-lint]x[/code] and [code skip-lint][url=address]text[/url][/code].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "Use `x` and `[url=address]text[/url]`.");
    }

    // Inline code keeps bracket roles literal.
    #[test]
    fn code__member_literal() {
        let description = "Literal [code][member Vector2.x][/code].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "Literal `[member Vector2.x]`.");
    }

    // Inline code spans keep bracketed type-like text literal.
    #[test]
    fn code__type_literal() {
        let description = "Literal [code][Node][/code].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "Literal `[Node]`.");
    }

    // Inline code keeps method roles literal.
    #[test]
    fn code__method_literal() {
        let description = "Literal [code][method lerp][/code].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "Literal `[method lerp]`.");
    }

    #[test]
    fn bold__with_code_and_member_role() {
        let description = "MIDI note release.\n\
            [b]Note:[/b] Some devices send [constant MIDI_MESSAGE_NOTE_ON] with [member InputEventMIDI.velocity] = [code]0[/code].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(
            actual,
            "MIDI note release.\n\n\
            **Note:** Some devices send `MIDI_MESSAGE_NOTE_ON` with \
            \\[member InputEventMIDI.velocity] = `0`."
        );
    }

    #[test]
    fn url__basic() {
        let description = "Controller docs vary; see \
            [url=https://example.com/spec]the spec[/url] for sliders, pedals, and similar inputs.";

        let actual = import_doc_for_test(description, None);

        assert_eq!(
            actual,
            "Controller docs vary; see [the spec](https://example.com/spec) for sliders, \
            pedals, and similar inputs."
        );
    }

    #[test]
    fn italic__basic() {
        let description = "The current instrument is often called [i]program[/i] or \
            [i]preset[/i] in MIDI docs.";

        let actual = import_doc_for_test(description, None);

        assert_eq!(
            actual,
            "The current instrument is often called _program_ or _preset_ in MIDI docs."
        );
    }

    #[test]
    fn param__nested_in_bold() {
        let description = "[b]Use [param count][/b].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "**Use `count`**.");
    }

    #[test]
    fn kbd__basic() {
        let description = "Press [kbd]Ctrl + S[/kbd].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "Press `Ctrl + S`.");
    }

    // Signal roles fall back to code-formatted text.
    #[test]
    fn signal__code_fallback() {
        let description = "Emit [signal pressed].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "Emit `pressed`.");
    }

    // Annotation roles fall back to code-formatted text.
    #[test]
    fn annotation__code_fallback() {
        let description = "Use [annotation @GDScript.@rpc].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "Use `@GDScript.@rpc`.");
    }

    // Constructor roles fall back to code-formatted text.
    #[test]
    fn constructor__code_fallback() {
        let description = "Create [constructor Transform2D].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "Create `Transform2D()`.");
    }

    // A type-like bracket directly followed by `(http...)` must stay a Markdown link, not a Rust doc link.
    #[test]
    fn type__followed_by_http_url() {
        let description = "See [Node](https://example.com) for details.";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "See [Node](https://example.com) for details.");
    }

    #[test]
    fn type__followed_by_plural_suffix() {
        let description = "Use [AnimationNode](s).";

        let actual = import_doc_for_test(description, None);

        assert_eq!(
            actual,
            "Use [`AnimationNode`][crate::classes::AnimationNode](s)."
        );
    }

    // Unterminated BBCode stays literal instead of falling through into type-link parsing.
    #[test]
    fn bbcode__unclosed_tag_literal() {
        let description = "[b]hello";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "[b]hello");
    }

    // Inline code keeps nested BBCode literal.
    #[test]
    fn code__nested_bold() {
        let description = "Use [code][b]x[/b][/code].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "Use `[b]x[/b]`.");
    }

    // Empty brackets `[]` are passed through untouched.
    #[test]
    fn empty_brackets() {
        let description = "Edge case: [].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "Edge case: [].");
    }

    // A `[param ...]` whose name is not an identifier, such as `foo-bar`, is left untouched.
    #[test]
    fn param__non_ident_left_literal() {
        let description = "Bad name [param foo-bar].";

        let actual = import_doc_for_test(description, None);

        assert_eq!(actual, "Bad name [param foo-bar].");
    }
}
