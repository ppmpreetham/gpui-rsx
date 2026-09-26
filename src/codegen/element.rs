//! Element code generation
//!
//! Converts RSX elements into GPUI method-chaining code:
//! - Base tag construction
//! - Automatic ID management (supports combining IDs with the `key` attribute)
//! - Child node aggregation optimization
//! - Fragment and For-loop support (stateful elements in a for loop must provide `id` or `key`)
//!
//! Optimizations:
//! - Cache `Ident::to_string()` to avoid redundant heap allocations
//! - Use match-based `is_stateful_attr()` instead of double linear scanning
//! - Use `lookup_tag_default()` instead of linear search with `.iter().find()`
//! - `generate_attr_methods` pushes directly to the caller's Vec
//! - Each child node independently generates a `.child()` call, avoiding uniform array type constraints
//! - Automatic IDs are based on source code span positions (line + column), remaining stable under incremental compilation

use super::attribute::{AttrHints, generate_attr_methods_with_mode, static_class_expr_needs_id};
use super::class::{ClassMode, parse_class_string_with_mode};
use super::tables::{
    is_stateful_attr, is_stateful_class, lookup_attr_flag_method, lookup_tag_default,
};
use crate::diagnostics::{for_loop_missing_key_error, missing_required_attribute_error};

#[derive(Default)]
struct AttrAnalysis {
    name: Option<String>,
    static_class: Option<String>,
    needs_id: bool,
}

impl AttrAnalysis {
    fn hints(&self) -> AttrHints<'_> {
        AttrHints {
            name: self.name.as_deref(),
            static_class: self.static_class.as_deref(),
        }
    }
}

fn analyze_attr(attr: &RsxAttribute) -> AttrAnalysis {
    match attr {
        RsxAttribute::Value { name, value: _ } if name == "id" || name == "key" => {
            AttrAnalysis::default()
        }
        RsxAttribute::Value { name, value } if name == "class" => {
            let static_class = if let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(lit_str),
                ..
            }) = value
            {
                Some(lit_str.value())
            } else {
                None
            };
            let needs_id = if let Some(class) = static_class.as_deref() {
                class.split_ascii_whitespace().any(is_stateful_class)
            } else {
                static_class_expr_needs_id(value)
            };

            AttrAnalysis {
                static_class,
                needs_id,
                ..AttrAnalysis::default()
            }
        }
        RsxAttribute::Value { name, .. } => {
            let name = name.to_string();
            let needs_id = is_stateful_attr(&name);
            AttrAnalysis {
                name: Some(name),
                needs_id,
                ..AttrAnalysis::default()
            }
        }
        RsxAttribute::Flag(name) if name == "styled" => AttrAnalysis::default(),
        RsxAttribute::Flag(name) => {
            let name = name.to_string();
            let needs_id = is_stateful_attr(&name)
                || lookup_attr_flag_method(&name).is_some_and(is_stateful_attr);
            AttrAnalysis {
                name: Some(name),
                needs_id,
                ..AttrAnalysis::default()
            }
        }
        RsxAttribute::StateClass { method, .. } => {
            let name = method.to_string();
            let needs_id = is_stateful_attr(&name);
            AttrAnalysis {
                name: Some(name),
                needs_id,
                ..AttrAnalysis::default()
            }
        }
        _ => AttrAnalysis::default(),
    }
}

use crate::parser::{RsxAttribute, RsxBody, RsxElement, RsxElementName, RsxNode};
use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use syn::spanned::Spanned;

type CodegenResult = Result<TokenStream, TokenStream>;

/// Generates GPUI code (entry point).
///
/// Converts the parsed RSX AST into type-safe GPUI code.
///
/// # Returns
/// - Single element: returns an expression implementing `IntoElement`
/// - Fragment: returns `Vec<impl IntoElement>`
pub fn generate_body_with_mode(body: &RsxBody, mode: ClassMode) -> TokenStream {
    generate_body_checked(body, mode).unwrap_or_else(|err| err)
}

pub fn generate_body_expansion_preview(body: &RsxBody, mode: ClassMode) -> String {
    generate_body_with_mode(body, mode).to_string()
}

fn generate_body_checked(body: &RsxBody, mode: ClassMode) -> CodegenResult {
    match body {
        RsxBody::Single(element) => generate_element_checked(element, false, mode),
        RsxBody::Fragment(children) => {
            let child_exprs: Vec<TokenStream> = children
                .iter()
                .map(|node| generate_node_checked(node, false, mode))
                .collect::<Result<_, _>>()?;
            // Fragments retain vec![] — the return type is user-facing API
            Ok(quote! { vec![#(#child_exprs),*] })
        }
    }
}

/// Generates code for a single child node.
///
/// Ensures generated code has proper type inference and supports the IntoElement trait.
fn generate_node_checked(node: &RsxNode, require_loop_key: bool, mode: ClassMode) -> CodegenResult {
    match node {
        RsxNode::Element(elem) => generate_element_checked(elem, require_loop_key, mode),
        // Expressions are automatically type-inferred; GPUI's .child() accepts impl IntoElement
        RsxNode::Expr(expr) => Ok(expr.to_token_stream()),
        RsxNode::Spread(expr) => Ok(expr.to_token_stream()),
        RsxNode::For {
            binding,
            iter,
            body,
        } => generate_for_loop_checked(binding, iter, body, mode),
    }
}

/// Generates iterator code for for-loops.
///
/// Single child node → `.map()`, multiple child nodes → `.flat_map()` + `AnyElement` array.
///
/// Multiple child nodes use `AnyElement` for type erasure before being placed in an array,
/// avoiding `Vec` allocations on each loop iteration while allowing mixed concrete element
/// types within the loop body (such as `div()` and custom components).
///
/// Safety check: all stateful elements in the loop body (including deeply nested ones)
/// must provide `id` or `key`; otherwise, each iteration would generate the same automatic ID,
/// leading to GPUI state collisions. Therefore, a compile error is emitted at this stage.
fn generate_for_loop_checked(
    binding: &syn::Pat,
    iter: &syn::Expr,
    body: &[RsxNode],
    mode: ClassMode,
) -> CodegenResult {
    let body_exprs: Vec<TokenStream> = body
        .iter()
        .map(|node| generate_node_checked(node, true, mode))
        .collect::<Result<_, _>>()?;
    if body_exprs.len() == 1 {
        let single = &body_exprs[0];
        Ok(quote! { (#iter).into_iter().map(|#binding| #single) })
    } else {
        Ok(quote! {
            (#iter).into_iter().flat_map(|#binding| [#((#body_exprs).into_any_element()),*])
        })
    }
}

/// Generates code for a single element.
///
/// Generates a method chain such as `div().id("x").flex().child(...)`,
/// rather than an assignment pattern like `let mut element = div(); element = element.flex();`.
///
/// Advantages of the method-chaining pattern:
/// - Consistent with GPUI idiomatic usage
/// - Correctly handles the `Div` → `Stateful<Div>` type transition (type changes after `.id()`)
fn generate_element_checked(
    element: &RsxElement,
    require_loop_key: bool,
    mode: ClassMode,
) -> CodegenResult {
    // Cache tag name string to avoid multiple to_string() heap allocations
    let tag_str = element.name.to_string();

    // Fast path: when there are no attributes and no children, skip all scans and return the base tag directly
    if element.attributes.is_empty() && element.children.is_empty() {
        if tag_str
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase())
        {
            return generate_component_call(&element.name, &[], &[], &[]);
        }
        return generate_tag(&tag_str, &element.name, None, None, None, None);
    }

    let has_base = element
        .attributes
        .iter()
        .any(|attr| matches!(attr, RsxAttribute::Value { name, .. } if name == "base"));
    if tag_str
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
        && !has_base
    {
        let attr_pairs: Vec<(&syn::Ident, &syn::Expr)> = element
            .attributes
            .iter()
            .filter_map(|attr| match attr {
                RsxAttribute::Value { name, value } => Some((name, value)),
                _ => None,
            })
            .collect();
        let flags: Vec<&syn::Ident> = element
            .attributes
            .iter()
            .filter_map(|attr| match attr {
                RsxAttribute::Flag(name) => Some(name),
                _ => None,
            })
            .collect();
        return generate_component_call(&element.name, &attr_pairs, &flags, &element.children);
    }

    // Single pass to extract all needed information while generating user attribute methods.
    let mut user_id = None;
    let mut user_key = None;
    let mut base_expr = None;
    let mut input_state = None;
    let mut img_source = None;
    let mut icon_name = None;
    let mut kbd_keys = None;
    let mut canvas_prepaint = None;
    let mut canvas_paint = None;
    let mut has_styled = false;
    let is_component = tag_str
        .chars()
        .next()
        .map_or(false, |c| c.is_ascii_uppercase());
    let mut needs_id = false;

    // Pre-allocate method chain capacity:
    // - Multiply each attribute by 2 (class attribute expands to 3-4 methods on average, other attributes to 1)
    // - Plus the number of children
    let mut methods: Vec<TokenStream> =
        Vec::with_capacity(element.attributes.len() * 2 + element.children.len());

    for attr in &element.attributes {
        match attr {
            RsxAttribute::Value { name, value } if name == "id" => {
                user_id = Some(value);
            }
            RsxAttribute::Value { name, value } if name == "key" => {
                user_key = Some(value);
            }
            RsxAttribute::Value { name, value } if tag_str == "icon" && name == "name" => {
                icon_name = Some(value);
            }
            RsxAttribute::Value { name, value } if tag_str == "kbd" && name == "keys" => {
                kbd_keys = Some(value);
            }
            RsxAttribute::Value { name, value } if name == "base" => {
                base_expr = Some(value);
            }
            RsxAttribute::Value { name, value }
                if (tag_str == "input" || tag_str == "textarea") && name == "state" =>
            {
                input_state = Some(value);
            }
            RsxAttribute::Value { name, value }
                if tag_str == "img" && (name == "src" || name == "source") =>
            {
                img_source = Some(value);
            }
            RsxAttribute::Value { name, value } if tag_str == "canvas" && name == "prepaint" => {
                canvas_prepaint = Some(value);
            }
            RsxAttribute::Value { name, value } if tag_str == "canvas" && name == "paint" => {
                canvas_paint = Some(value);
            }
            RsxAttribute::Value { name, value } if tag_str == "Activity" && name == "mode" => {
                methods.push(quote! {
                    .map(|__el| {
                        if #value == "visible" {
                            __el.visible()
                        } else {
                            __el.invisible()
                        }
                    })
                });
            }
            RsxAttribute::Value { name, value } if tag_str == "svg" && name == "src" => {
                methods.push(quote! { .path(#value) });
            }
            RsxAttribute::Flag(name) if name == "styled" => {
                has_styled = true;
            }
            _ => {
                let analysis = analyze_attr(attr);
                if !needs_id && analysis.needs_id && !is_component {
                    needs_id = true;
                }
                generate_attr_methods_with_mode(attr, analysis.hints(), &mut methods, mode);
            }
        }
    }

    if require_loop_key && needs_id && user_id.is_none() && user_key.is_none() {
        return Err(for_loop_missing_key_error(&element.name.path, &tag_str).to_compile_error());
    }

    // Generate base element and id:
    //  1. Explicit id              → Use directly, highest priority
    //  2. Needs id + key exists    → Auto-ID prefix + key (concatenated at runtime, ensures uniqueness in loops)
    //  3. Needs id, no key         → Auto-ID based purely on source location
    //  4. Does not need id         → Do not inject (key is silently ignored in this case)
    let tag = if tag_str == "kbd" {
        if let Some(keys) = kbd_keys {
            quote! { gpui_kit::component::kbd::Kbd::new(gpui_kit::Keystroke::parse(#keys).unwrap()) }
        } else {
            quote! { gpui_kit::component::kbd::Kbd::new(gpui_kit::Keystroke::parse("unknown").unwrap()) }
        }
    } else if tag_str == "button" || tag_str == "button_group" {
        let btn_id = if let Some(id_value) = user_id {
            quote! { #id_value }
        } else if let Some(key_expr) = user_key {
            make_keyed_auto_id(&element.name, key_expr)
        } else {
            make_auto_id(&element.name)
        };
        needs_id = false;
        user_id = None;
        user_key = None;
        if tag_str == "button" {
            quote! { gpui_kit::component::button::Button::new(#btn_id) }
        } else {
            quote! { gpui_kit::component::button::ButtonGroup::new(#btn_id) }
        }
    } else if let Some(base) = base_expr {
        quote! { #base }
    } else if let Some(state) = input_state {
        if tag_str == "input" {
            quote! { gpui_kit::component::input::Input::new(#state) }
        } else {
            quote! { gpui_kit::component::input::Textarea::new(#state) }
        }
    } else {
        generate_tag(
            &tag_str,
            &element.name,
            img_source,
            canvas_prepaint,
            canvas_paint,
            icon_name,
        )?
    };
    let base = if let Some(id_value) = user_id {
        quote! { #tag.id(#id_value) }
    } else if needs_id {
        if let Some(key_expr) = user_key {
            let keyed_id = make_keyed_auto_id(&element.name, key_expr);
            quote! { #tag.id(#keyed_id) }
        } else {
            let auto_id = make_auto_id(&element.name);
            quote! { #tag.id(#auto_id) }
        }
    } else {
        tag
    };

    // styled flag → inject tag default styles (before user attributes)
    let default_methods: Vec<TokenStream> =
        if has_styled && let Some(class_str) = lookup_tag_default(&tag_str) {
            parse_class_string_with_mode(class_str, mode).collect()
        } else {
            Vec::new()
        };

    // Child nodes → .child() / .children() calls (including aggregation optimization)
    generate_children_methods(&element.children, require_loop_key, &mut methods, mode)?;

    Ok(quote! { #base #(#default_methods)* #(#methods)* })
}

/// Generates method-chaining fragments for child nodes.
fn generate_children_methods(
    children: &[RsxNode],
    require_loop_key: bool,
    methods: &mut Vec<TokenStream>,
    mode: ClassMode,
) -> Result<(), TokenStream> {
    for node in children {
        match node {
            RsxNode::Expr(expr) => {
                methods.push(quote! { .child(#expr) });
            }
            RsxNode::Element(elem) => {
                let child_expr = generate_element_checked(elem, require_loop_key, mode)?;
                methods.push(quote! { .child(#child_expr) });
            }
            RsxNode::Spread(expr) => {
                methods.push(quote! { .children(#expr) });
            }
            RsxNode::For {
                binding,
                iter,
                body,
            } => {
                let for_expr = generate_for_loop_checked(binding, iter, body, mode)?;
                methods.push(quote! { .children(#for_expr) });
            }
        }
    }
    Ok(())
}

/// HTML tag → `div()`, special tag → function of the same name, custom component → function call of the same name
///
/// Accepts pre-cached `tag_str` to avoid repeated `to_string()` calls
fn generate_component_call(
    name: &RsxElementName,
    attrs: &[(&syn::Ident, &syn::Expr)],
    flags: &[&syn::Ident],
    children: &[RsxNode],
) -> CodegenResult {
    if name.as_single_ident().is_none() {
        return Err(syn::Error::new(
            name.span(),
            "component tags with paths (e.g. `<ui::TaskCard />`) are not supported yet; use a single-identifier tag",
        )
        .to_compile_error());
    }

    let props_ident = {
        let ident = name.as_single_ident().unwrap();
        let mut s = ident.to_string();
        s.push_str("Props");
        proc_macro2::Ident::new(&s, ident.span())
    };

    let mut setters = TokenStream::new();
    for (attr_name, expr) in attrs {
        setters.extend(quote! { .#attr_name(#expr) });
    }
    for flag in flags {
        setters.extend(quote! { .#flag() });
    }

    let children_setter = if children.is_empty() {
        TokenStream::new()
    } else {
        let child_exprs: Vec<TokenStream> = children
            .iter()
            .map(|node| match node {
                RsxNode::Element(elem) => {
                    generate_element_checked(elem, false, ClassMode::Permissive)
                }
                RsxNode::Expr(expr) => Ok(quote! { #expr }),
                RsxNode::Spread(expr) => Err(syn::Error::new(
                    expr.span(),
                    "spread syntax is not supported in component children",
                )
                .to_compile_error()),
                RsxNode::For { .. } => Err(syn::Error::new(
                    name.span(),
                    "for-loop children are not supported in component tags yet",
                )
                .to_compile_error()),
            })
            .collect::<Result<Vec<_>, _>>()?;
        quote! { .children(vec![#(#child_exprs),*]) }
    };

    let loc = name.span().start();
    let (line, column) = (loc.line as u64, loc.column as u64);
    Ok(quote! {
        #props_ident::new() #setters #children_setter .render_at(
            gpui_kit::ElementId::NamedInteger(
                concat!(file!(), "::__zopra_tag_").into(),
                #line * 10_000 + #column,
            ),
            window,
            cx,
        )
    })
}

fn generate_tag(
    tag_str: &str,
    name: &RsxElementName,
    img_source: Option<&syn::Expr>,
    canvas_prepaint: Option<&syn::Expr>,
    canvas_paint: Option<&syn::Expr>,
    icon_name: Option<&syn::Expr>,
) -> CodegenResult {
    if name.as_single_ident().is_none() {
        let path = &name.path;
        return Ok(quote! { #path() });
    }

    let path = &name.path;
    Ok(match tag_str {
        // Special tags: kept as function calls with the same name
        "svg" => quote! { gpui_kit::svg() },
        "icon" => {
            if let Some(name) = icon_name {
                quote! { gpui_kit::component::Icon::new(#name) }
            } else {
                quote! { gpui_kit::component::Icon::default() }
            }
        }
        "img" => {
            let Some(source) = img_source else {
                return Err(missing_required_attribute_error(
                    &name.path,
                    "img",
                    "src",
                    r#"<img src={"path/to/image.png"} />"#,
                )
                .to_compile_error());
            };
            quote! { img(#source) }
        }
        "canvas" => {
            let Some(prepaint) = canvas_prepaint else {
                return Err(missing_required_attribute_error(
                    &name.path,
                    "canvas",
                    "prepaint",
                    r#"<canvas prepaint={|bounds, window, cx| state} paint={|bounds, state, window, cx| { ... }} />"#,
                )
                .to_compile_error());
            };
            let Some(paint) = canvas_paint else {
                return Err(missing_required_attribute_error(
                    &name.path,
                    "canvas",
                    "paint",
                    r#"<canvas prepaint={|bounds, window, cx| state} paint={|bounds, state, window, cx| { ... }} />"#,
                )
                .to_compile_error());
            };
            quote! { canvas(#prepaint, #paint) }
        }
        // HTML tags: uniformly mapped to div()
        "div" | "span" | "section" | "article" | "header" | "footer" | "main" | "nav" | "aside"
        | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "p" | "label" | "a" | "input"
        | "textarea" | "select" | "form" | "ul" | "ol" | "li" | "kbd" | "Activity" => {
            quote! { div() }
        }
        _ => quote! { #path() },
    })
}

/// Generates a stable automatic ID based on source location (without key).
///
/// Format: `concat!(file!(), "::", "__rsx_{tag}_L{line}C{col}")`
///
/// **Stability**: As long as the element's source location does not change, the ID remains unchanged (incremental compilation safe).
/// **Uniqueness**: `file!()` expands at the call site, containing the full path, globally unique across files.
///
/// If an ID completely stable across refactoring is required, use the `id` attribute;
/// If used inside a loop, use the `key` attribute instead.
fn make_auto_id(tag_name: &RsxElementName) -> TokenStream {
    let span = tag_name.span();
    let loc = span.start(); // Requires proc-macro2 span-locations feature
    let id_suffix = format!("__rsx_{}_L{}C{}", tag_name, loc.line, loc.column);
    quote! { concat!(file!(), "::", #id_suffix) }
}

/// Generates a composite automatic ID with `key` (for loop scenarios).
///
/// Dynamic key format: `format!("{file}::{prefix}_{key}", file!(), key_expr)`
/// Literal key format: `concat!(file!(), "::{prefix}_", "literal")`
///
/// `concat!(file!(), ...)` is evaluated at compile time (zero cost). Dynamic `key_expr` is concatenated at runtime,
/// ensuring each element in the same loop iteration receives a unique ID.
/// `key_expr` must implement `std::fmt::Display` (numbers, strings, and custom types are all supported).
fn make_keyed_auto_id(tag_name: &RsxElementName, key_expr: &syn::Expr) -> TokenStream {
    let span = tag_name.span();
    let loc = span.start();
    // Compile-time constant prefix containing file path + source location, formatted as:
    //   "src/views/list.rs::__rsx_li_L42C8_"
    let prefix_suffix = format!("::__rsx_{}_L{}C{}_", tag_name, loc.line, loc.column);
    if let Some(static_suffix) = static_key_suffix(key_expr) {
        return quote! { concat!(file!(), #prefix_suffix, #static_suffix) };
    }
    // Append key to prefix at runtime, producing e.g.:
    //   "src/views/list.rs::__rsx_li_L42C8_item_42"
    quote! { format!(concat!(file!(), #prefix_suffix, "{}"), #key_expr) }
}

fn static_key_suffix(expr: &syn::Expr) -> Option<String> {
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(lit),
            ..
        }) => Some(lit.value()),
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(lit),
            ..
        }) => lit
            .base10_parse::<u128>()
            .ok()
            .map(|value| value.to_string()),
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Bool(lit),
            ..
        }) => Some(lit.value.to_string()),
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Char(lit),
            ..
        }) => Some(lit.value().to_string()),
        syn::Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Neg(_)) => match &*unary.expr {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Int(lit),
                ..
            }) => lit
                .base10_parse::<u128>()
                .ok()
                .map(|value| format!("-{value}")),
            _ => None,
        },
        syn::Expr::Paren(expr) => static_key_suffix(&expr.expr),
        syn::Expr::Group(expr) => static_key_suffix(&expr.expr),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element_name(name: &str) -> RsxElementName {
        RsxElementName {
            path: syn::parse_str(name).expect("valid element name"),
        }
    }

    #[test]
    fn img_requires_source() {
        let error = generate_tag("img", &element_name("img"), None, None, None, None)
            .expect_err("img without src must fail")
            .to_string();

        assert!(error.contains("Element `<img>` requires `src`"));
    }

    #[test]
    fn canvas_requires_prepaint_and_paint() {
        let callback: syn::Expr = syn::parse_quote!(callback);
        let name = element_name("canvas");

        let missing_prepaint = generate_tag("canvas", &name, None, None, Some(&callback), None)
            .expect_err("canvas without prepaint must fail")
            .to_string();
        assert!(missing_prepaint.contains("Element `<canvas>` requires `prepaint`"));

        let missing_paint = generate_tag("canvas", &name, None, Some(&callback), None, None)
            .expect_err("canvas without paint must fail")
            .to_string();
        assert!(missing_paint.contains("Element `<canvas>` requires `paint`"));
    }

    #[test]
    fn static_key_suffix_supports_literal_display_types() {
        let cases: [(syn::Expr, &str); 6] = [
            (syn::parse_quote!("item"), "item"),
            (syn::parse_quote!(42), "42"),
            (syn::parse_quote!(true), "true"),
            (syn::parse_quote!('x'), "x"),
            (syn::parse_quote!(-7), "-7"),
            (syn::parse_quote!((9)), "9"),
        ];

        for (expr, expected) in cases {
            assert_eq!(static_key_suffix(&expr).as_deref(), Some(expected));
        }
    }

    #[test]
    fn static_key_suffix_keeps_dynamic_values_at_runtime() {
        let dynamic: syn::Expr = syn::parse_quote!(item.id);
        let non_integer_negative: syn::Expr = syn::parse_quote!(-1.5);

        assert_eq!(static_key_suffix(&dynamic), None);
        assert_eq!(static_key_suffix(&non_integer_negative), None);
    }
}





