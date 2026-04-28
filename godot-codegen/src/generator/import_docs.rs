/*
 * Copyright (c) godot-rust; Bromeon and contributors.
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::fmt::Write;

use crate::context::Context;
use crate::models::domain::{ApiView, Class, ClassLike, Function, TyName};
use crate::{conv, special_cases, util};

type CowStr = std::borrow::Cow<'static, str>;

pub fn import_docs(
    description: &str,
    surrounding_class: Option<&Class>,
    ctx: &Context,
    view: &ApiView,
) -> String {
    let mut result = replace_simple_tags(description, ctx);
    result = replace_param_links(&result, ctx);
    result = replace_type_links(&result, surrounding_class, ctx);
    result = replace_method_links(&result, surrounding_class, ctx, view);
    result = replace_unimplemented_links(&result, ctx);

    result
}

pub fn import_function_docs(fun: &dyn Function, ctx: &Context, view: &ApiView) -> Option<String> {
    let doc = fun.common().description.as_ref()?;
    if doc.is_empty() {
        return None;
    }
    let surrounding_class_name = fun.surrounding_class();
    let surrounding_class = surrounding_class_name.and_then(|name| view.find_engine_class(name));
    let imported_doc = import_docs(doc, surrounding_class, ctx, view);
    let imported_doc = format!("\n# Godot docs\n{imported_doc}");
    Some(imported_doc)
}

fn replace_unimplemented_links(str: &str, ctx: &Context) -> String {
    ctx.regexes()
        .unimplemented_links
        .replace_all(str, "\\$0")
        .to_string()
}

fn replace_simple_tags(str: &str, ctx: &Context) -> String {
    let re = ctx.regexes();

    // Replace \n with \n\n everywhere except codeblock tags.
    let result = re.newlines.replace_all(str, "$1$3$3");

    let result = re.bold_tags.replace_all(&result, "**$1**");
    let result = re.italic_tags.replace_all(&result, "_${1}_");
    let result = re.code_tags.replace_all(&result, "`$2`");
    let result = re.kbd_tags.replace_all(&result, "`$1`");
    let result = re.url_tags.replace_all(&result, "[$2]($1)");
    let result = re.codeblocks_tags.replace_all(&result, "$1");
    let result = re.codeblock_tags.replace_all(&result, "```gdscript$1```");
    let result = re.codeblock_lang_tags.replace_all(&result, "```$1$2```");
    let result = re.gdscript_tags.replace_all(&result, "```gdscript$1```");
    let result = re.csharp_tags.replace_all(&result, "```csharp$1```");

    result.to_string()
}

fn replace_param_links(str: &str, ctx: &Context) -> String {
    ctx.regexes()
        .param_links
        .replace_all(str, "`$1`")
        .to_string()
}

fn replace_type_links(doc: &str, surrounding_class: Option<&Class>, ctx: &Context) -> String {
    let mut result = String::new();
    let mut previous = 0;
    for captures in ctx.regexes().type_links.captures_iter(doc) {
        let whole_match = captures.get(0).unwrap();
        let start = whole_match.start();
        let end = whole_match.end();
        if doc[end..].starts_with("(http") {
            continue;
        }

        // Type link regex captures markdown `code`, ```codeblock``` or Godot docs type link [Node],
        // this is to prevent replacing what looks like a type link inside the code (`array[0]`, `dictionary[variable]`).
        // This is why class name is a capture group 3.
        // If there is no class name in the capture, in other words we matched a codeblock, not a type link, then we skip it.
        let Some(class_name) = captures.get(3) else {
            continue;
        };

        let class_name = class_name.as_str();
        result.push_str(&doc[previous..start]);

        // If we encounter a deleted or primitive type, or an ignored link,
        // we insert it without any links or formatting.
        if special_cases::is_godot_type_deleted(class_name)
            || matches_primitive_type(class_name)
            || matches_ignored_links(class_name)
        {
            write!(result, "{class_name}").unwrap();
        } else if class_name == "@GlobalScope" {
            write!(result, "[@GlobalScope][crate::global]").unwrap();
        } else {
            let path = get_class_rust_path(class_name, ctx);
            let is_link_to_surrounding_class = surrounding_class.is_some_and(|class| {
                let current_class_name = class.name().rust_ty.to_string();
                current_class_name == conv::to_pascal_case(class_name)
            });

            // If a link points to the current class, then do not create a link tag in Markdown to reduce noise.
            if is_link_to_surrounding_class {
                write!(result, "`{class_name}`").unwrap();
            } else {
                write!(result, "[{class_name}][{path}]").unwrap();
            }
        }
        previous = end;
    }
    result.push_str(&doc[previous..]);
    result
}

fn matches_primitive_type(ty: &str) -> bool {
    matches!(ty, "int" | "float" | "bool")
}

fn matches_ignored_links(class: &str) -> bool {
    // We don't have a single place to point @GDScript to.
    class == "@GDScript"
}

fn replace_method_links(
    doc: &str,
    surrounding_class: Option<&Class>,
    ctx: &Context,
    view: &ApiView,
) -> String {
    let mut result = String::new();
    let mut previous = 0;

    for captures in ctx.regexes().method_links.captures_iter(doc) {
        let whole_match = captures.get(0).unwrap();
        let start = whole_match.start();
        let end = whole_match.end();
        if doc[end..].starts_with("(http") {
            continue;
        }
        result.push_str(&doc[previous..start]);
        let method_path = captures.get(1).unwrap().as_str();

        if let Some(method_path) = convert_to_method_path(method_path, surrounding_class, ctx, view)
        {
            let (_, method_name) = method_path
                .rsplit_once("::")
                .expect("rsplit_once should return a method name");
            write!(result, "[{method_name}][`{method_path}`]").unwrap();
        } else {
            write!(result, "\\{}", whole_match.as_str()).unwrap();
        }

        previous = end;
    }
    result.push_str(&doc[previous..]);

    result
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

    if let (true, ret) = matches_hardcoded_method(link_godot_class, &link_godot_method) {
        return ret;
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
            if class.is_final {
                // Final classes don't have an associated trait with virtual methods.
                return None;
            } else {
                let path = format!(
                    "crate::classes::{}::{}",
                    class.name().virtual_trait_name(),
                    rust_method_name
                );
                return Some(path.into());
            }
        }
    }

    let godot_method_name = link_godot_method.trim_start_matches("_");
    let rust_class_path = get_class_rust_path(link_godot_class, ctx);
    Some(format!("{rust_class_path}::{godot_method_name}").into())
}

fn matches_hardcoded_method(godot_class: &str, godot_method: &str) -> (bool, Option<CowStr>) {
    let path = match (godot_class, godot_method) {
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
        ("@GlobalScope", _) => format!("crate::global::{}", godot_method).into(),
        ("@GDScript", _) => return (true, None),
        _ => return (false, None),
    };

    (true, Some(path))
}

fn convert_builtin_types(type_name: &str) -> Option<&'static str> {
    match type_name {
        "String" => Some("crate::builtin::GString"),
        "Array" => Some("crate::builtin::Array"),
        "Dictionary" => Some("crate::builtin::Dictionary"),
        _ => None,
    }
}

fn get_class_rust_path(godot_class_name: &str, ctx: &Context) -> CowStr {
    if let Some(hardcoded_builtin_type) = convert_builtin_types(godot_class_name) {
        return hardcoded_builtin_type.into();
    }

    let is_builtin = ctx.is_builtin(godot_class_name);
    let rust_class_name = crate::conv::to_pascal_case(godot_class_name);
    if is_builtin {
        format!("crate::builtin::{rust_class_name}").into()
    } else {
        format!("crate::classes::{rust_class_name}").into()
    }
}
