//! Class string parsing
//!
//! Parses CSS class strings into GPUI method call chains, supporting:
//! - Tailwind-style utility classes (flex, gap-4, text-red-500)
//! - Arbitrary hex color values (bg-[#ff0000])
//! - Spacing/sizing classes
//!
//! Core optimizations:
//! - Unified color parsing function to avoid code duplication
//! - Spacing prefixes use rfind + match (O(1)) instead of linear scanning (O(17))
//! - `text_` prefix performs strip_prefix only once, consolidating color and text size branch handling
//! - Text size uses match instead of contains linear search
//! - Common numeric length, color, opacity, and line-clamp classes are parsed directly on the original class to reduce string allocations
//! - Check `contains('-')` first to bypass heap allocation for hyphen-free classes; full replacement with `replace` when hyphens are present
//! - split_ascii_whitespace replaces split_whitespace (class names contain ASCII only)

use super::tables::*;
use proc_macro2::{Span, TokenStream, TokenTree};
use quote::quote;
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClassMode {
    Permissive,
    Strict,
}

impl ClassMode {
    fn is_strict(self) -> bool {
        matches!(self, Self::Strict)
    }
}

/// Parses a class string into an iterator of method chain fragments
///
/// `"flex flex-col gap-4"` → `[.flex(), .flex_col(), .gap(rems(1.0))]`
///
/// Numeric values are Tailwind scale steps: `n` becomes `rems(n / 4)` so every
/// length a class produces scales with the window's rem. Only explicit units
/// (`w-[280px]`, `w-[18rem]`) escape the rem.
///
/// Fragments are mostly method calls (`.flex()`); the rest are [`wrap`]s, so callers
/// apply them with [`chain`] rather than pasting them after an expression.
pub(crate) fn parse_class_string_with_mode(
    class_str: &str,
    mode: ClassMode,
) -> impl Iterator<Item = TokenStream> {
    let mut plain = Vec::new();
    let mut states: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut conditions: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut errors = Vec::new();

    for class in class_str.split_ascii_whitespace() {
        let Some((variant, rest)) = class.split_once(':') else {
            plain.push(class);
            continue;
        };
        let groups = if STATES.contains(&variant) {
            if let Some((inner, _)) = rest.split_once(':')
                && condition(inner).is_some()
            {
                errors.push(compile_error(format!(
                    "`{class}`: write `{inner}:` before `{variant}:`, e.g. `{inner}:{variant}:...`."
                )));
                continue;
            }
            &mut states
        } else if condition(variant).is_some() {
            &mut conditions
        } else if is_unsupported_tailwind_variant(variant) {
            errors.push(warning(format!(
                "`{class}` is ignored: GPUI has no `{variant}:` state yet. Supported variants: \
                 hover, active, focus, group-hover, dark, sm .. 2xl, max-sm .. max-2xl; \
                 otherwise use `when={{..}}` with the class's attribute."
            )));
            continue;
        } else {
            errors.push(compile_error(format!(
                "Unknown variant `{variant}:` in `{class}`. Supported: hover, active, focus, \
                 group-hover, dark, sm, md, lg, xl, 2xl, and max-sm .. max-2xl."
            )));
            continue;
        };
        match groups.iter_mut().find(|(name, _)| *name == variant) {
            Some((_, classes)) => classes.push(rest),
            None => groups.push((variant, vec![rest])),
        }
    }

    let mut result = parse_plain(&plain, mode);
    for (state, classes) in states {
        let method = syn::Ident::new(&state.replace('-', "_"), Span::call_site());
        let style = chain(quote! { style }, parse_plain(&classes, mode));
        result.push(quote! { .#method(|style| #style) });
    }
    for (variant, classes) in conditions {
        let test = condition(variant);
        let inner = chain(
            quote! { __el },
            parse_class_string_with_mode(&classes.join(" "), mode),
        );
        result.push(wrap(quote! { if #test { #inner } else { __el } }));
    }
    result.extend(errors);
    result.into_iter()
}

/// Parses a single CSS class (variants included) into chain fragments.
pub(crate) fn parse_single_class_with_mode(class: &str, mode: ClassMode) -> TokenStream {
    parse_class_string_with_mode(class, mode).collect()
}

const STATES: &[&str] = &["hover", "active", "focus", "group-hover"];

/// The test behind a `dark:` or breakpoint variant. Breakpoints are Tailwind's, in rems.
fn condition(variant: &str) -> Option<TokenStream> {
    if variant == "dark" {
        return Some(quote! { gpui_kit::component::Theme::global(cx).is_dark() });
    }
    let (below, breakpoint) = match variant.strip_prefix("max-") {
        Some(breakpoint) => (true, breakpoint),
        None => (false, variant),
    };
    let rems: f32 = match breakpoint {
        "sm" => 40.0,
        "md" => 48.0,
        "lg" => 64.0,
        "xl" => 80.0,
        "2xl" => 96.0,
        _ => return None,
    };
    Some(if below {
        quote! { window.viewport_size().width < window.rem_size() * #rems }
    } else {
        quote! { window.viewport_size().width >= window.rem_size() * #rems }
    })
}

/// A fragment that isn't a method call: `body` sees the element so far as `__el` and
/// evaluates to the element.
pub(crate) fn wrap(body: TokenStream) -> TokenStream {
    quote! { __rsx_wrap(#body) }
}

/// Applies class fragments to `base`: method calls are appended, [`wrap`]s rebind it.
pub(crate) fn chain(
    base: TokenStream,
    fragments: impl IntoIterator<Item = TokenStream>,
) -> TokenStream {
    let mut acc = base;
    let mut tokens = fragments.into_iter().flatten().peekable();
    while let Some(token) = tokens.next() {
        if let TokenTree::Ident(ident) = &token
            && ident == "__rsx_wrap"
            && let Some(TokenTree::Group(body)) = tokens.peek()
        {
            let body = body.stream();
            tokens.next();
            acc = quote! { ({ #[allow(unused_mut)] let mut __el = #acc; #body }) };
        } else {
            acc.extend([token]);
        }
    }
    acc
}

/// Variant-free classes. Gradient stops and shadow layers combine across classes, so
/// those are gathered and emitted once.
fn parse_plain(classes: &[&str], mode: ClassMode) -> Vec<TokenStream> {
    let mut gradient = Gradient::default();
    let mut shadows = Shadows::default();
    let mut result: Vec<_> = classes
        .iter()
        .filter(|class| !gradient.take(class) && !shadows.take(class))
        .map(|class| parse_atom(class, mode))
        .collect();
    result.extend(gradient.finish());
    result.extend(shadows.finish());
    result
}

fn parse_atom(class: &str, mode: ClassMode) -> TokenStream {
    if let Some(token) = parse_font_family_class(class) {
        return token;
    }

    if let Some(token) = parse_font_weight_class(class, mode) {
        return token;
    }

    if let Some(token) = parse_object_fit_class(class) {
        return token;
    }

    if class == "debug-outline" {
        return wrap(quote! {
            #[cfg(debug_assertions)]
            let __el = __el.debug();
            __el
        });
    }

    if let Some(token) = parse_text_with_leading_class(class)
        .or_else(|| parse_leading_class(class))
        .or_else(|| parse_grid_class(class))
        .or_else(|| parse_place_class(class))
        .or_else(|| parse_flex_factor_class(class))
        .or_else(|| parse_overflow_class(class))
        .or_else(|| parse_scrollbar_class(class))
        .or_else(|| parse_rounded_class(class))
    {
        return token;
    }

    if let Some(token) = parse_tailwind_alias_class(class) {
        return token;
    }

    if let Some(token) = parse_arbitrary_length_class(class) {
        return token;
    }

    if let Some(token) = parse_fraction_length_class(class) {
        return token;
    }

    if let Some(token) = parse_numeric_length_class(class) {
        return token;
    }

    if let Some(token) = parse_arbitrary_bracket_length_class(class) {
        return token;
    }

    if let Some(token) = parse_direct_color_class(class) {
        return token;
    }

    if let Some(token) = parse_direct_opacity_class(class) {
        return token;
    }

    if let Some(token) = parse_direct_line_clamp_class(class) {
        return token;
    }

    if let Some(token) = parse_direct_directional_border_class(class) {
        return token;
    }

    // Allocate a new String when '-' or '/' is present; borrow the original string with zero-copy otherwise.
    // Tailwind fraction classes like `w-1/2` map to GPUI helpers like `w_1_2`.
    let method_name: Cow<str> = if class.contains(['-', '/']) {
        Cow::Owned(class.replace(['-', '/'], "_"))
    } else {
        Cow::Borrowed(class)
    };

    // Spacing/sizing classes: use rfind('_') + match for O(1) prefix lookup
    if let Some(underscore_pos) = method_name.rfind('_') {
        let suffix = &method_name[underscore_pos + 1..];
        if let Some(num) = parse_length_number(suffix) {
            let prefix = &method_name[..=underscore_pos];
            if let Some(method) = lookup_spacing_method(prefix) {
                let method_ident = syn::Ident::new(method, Span::call_site());
                let rem_val = num * 0.25;
                return quote! { .#method_ident(gpui_kit::rems(#rem_val)) };
            }
        }
    }

    // Directional border zero-arg width methods in GPUI 0.2 carry a `_1` suffix.
    if let Some(method) = lookup_directional_border_method(&method_name) {
        let ident = syn::Ident::new(method, Span::call_site());
        return quote! { .#ident() };
    }

    // Special handling for border:
    // "border" (plain) → .border_1() (GPUI has no parameterless .border())
    // "border-2" → .border_2()
    if method_name == "border" {
        return quote! { .border_1() };
    }

    if method_name == "rounded_theme" {
        return quote! { .rounded(gpui_kit::component::Theme::global(cx).radius) };
    }

    if method_name == "rounded_theme_lg" {
        return quote! { .rounded(gpui_kit::component::Theme::global(cx).radius_lg) };
    }

    if method_name == "no_underline" {
        return quote! { .text_decoration_none() };
    }

    // border-color class: border-red-500 → .border_color(rgb(0xef4444))
    if let Some(rest) = method_name.strip_prefix("border_")
        && !is_directional_border(rest)
    {
        if rest.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
            // Numeric border width: border-2, border-4, etc.
            let ident = syn::Ident::new(&method_name, Span::call_site());
            return quote! { .#ident() };
        } else if let Some(token) = parse_color_with_method(rest, "border_color", class) {
            return token;
        }
    }

    // text_ prefix: unified handling of color classes (text-red-600) and text size classes (text-xl)
    // Strip prefix "text_" only once to avoid doing it separately in color and size branches.
    if let Some(rest) = method_name.strip_prefix("text_") {
        // Look up color table first (text-red-500 → .text_color(rgb(...)))
        if let Some(token) = parse_color_with_method(rest, "text_color", class) {
            return token;
        }
        // Then look up text size (text-xl → .text_xl())
        if let Some(token) = text_size_tokens(rest) {
            return token;
        }
        // Non-whitelisted text_ prefixes fall through to default handling
    }

    // bg_ color class: bg-blue-500 → .bg(rgb(...))
    if let Some(rest) = method_name.strip_prefix("bg_")
        && let Some(token) = parse_color_with_method(rest, "bg", class)
    {
        return token;
    }

    // opacity_ class: opacity-50 → .opacity(0.5)
    // Note: GPUI's opacity range is 0.0–1.0, while Tailwind uses 0–100 integers
    if let Some(rest) = method_name.strip_prefix("opacity_")
        && let Ok(n) = rest.parse::<u8>()
    {
        let val = n as f32 / 100.0;
        return quote! { .opacity(#val) };
    }

    // line-clamp-3 → .line_clamp(3usize)
    if let Some(rest) = method_name.strip_prefix("line_clamp_")
        && let Ok(lines) = rest.parse::<usize>()
    {
        return quote! { .line_clamp(#lines) };
    }

    // Known no-argument GPUI methods; anything else is a clear error rather than rustc's
    // "no method named ..." pointing at the macro.
    if is_supported_common_no_arg_class(class) {
        let ident = syn::Ident::new(&method_name, Span::call_site());
        quote! { .#ident() }
    } else if let Some(reason) = unsupported_tailwind_class(class) {
        // Real Tailwind that GPUI can't draw yet: warn and skip, so the rest still builds.
        warning(format!("`{class}` is ignored: GPUI doesn't support it yet. {reason}"))
    } else {
        compile_error(unsupported_class_message(class, &method_name))
    }
}

fn text_size_tokens(size: &str) -> Option<TokenStream> {
    if !is_valid_text_size(size) {
        return None;
    }
    let rems: f32 = match size {
        "4xl" => 2.25,
        "5xl" => 3.0,
        "6xl" => 3.75,
        "7xl" => 4.5,
        "8xl" => 6.0,
        "9xl" => 8.0,
        _ => {
            let ident = syn::Ident::new(&format!("text_{size}"), Span::call_site());
            return Some(quote! { .#ident() });
        }
    };
    Some(quote! { .text_size(gpui_kit::rems(#rems)) })
}

/// `text-sm/6`: a font size with its line height.
fn parse_text_with_leading_class(class: &str) -> Option<TokenStream> {
    let (size, leading) = class.strip_prefix("text-")?.split_once('/')?;
    let size = text_size_tokens(size)?;
    Some(match leading_tokens(leading) {
        Some(leading) => quote! { #size .line_height(#leading) },
        None => compile_error(invalid_leading_message(class)),
    })
}

fn parse_leading_class(class: &str) -> Option<TokenStream> {
    let leading = class.strip_prefix("leading-")?;
    Some(match leading_tokens(leading) {
        Some(leading) => quote! { .line_height(#leading) },
        None => compile_error(invalid_leading_message(class)),
    })
}

fn leading_tokens(leading: &str) -> Option<TokenStream> {
    let ratio = match leading {
        "none" => Some(1.0f32),
        "tight" => Some(1.25),
        "snug" => Some(1.375),
        "normal" => Some(1.5),
        "relaxed" => Some(1.625),
        "loose" => Some(2.0),
        _ => None,
    };
    if let Some(ratio) = ratio {
        return Some(quote! { gpui_kit::relative(#ratio) });
    }
    if let Some(steps) = parse_length_number(leading) {
        let rems = steps * 0.25;
        return Some(quote! { gpui_kit::rems(#rems) });
    }
    let inner = leading.strip_prefix('[')?.strip_suffix(']')?;
    if let Some(ratio) = parse_length_number(inner) {
        return Some(quote! { gpui_kit::relative(#ratio) });
    }
    match LengthKind::parse_arbitrary(leading, inner).ok()? {
        LengthKind::Px(value) => Some(quote! { gpui_kit::px(#value) }),
        LengthKind::Rem(value) => Some(quote! { gpui_kit::rems(#value) }),
        LengthKind::Relative(value) => Some(quote! { gpui_kit::relative(#value) }),
    }
}

fn invalid_leading_message(class: &str) -> String {
    format!(
        "Invalid line height in `{class}`. Expected none, tight, snug, normal, relaxed, loose, \
         a spacing step like `6`, or `[1.4]` / `[20px]` / `[1.25rem]`."
    )
}

/// `font-sans` / `font-mono` follow the theme's fonts.
fn parse_font_family_class(class: &str) -> Option<TokenStream> {
    match class {
        "font-sans" => Some(quote! {
            .font_family(gpui_kit::component::Theme::global(cx).font_family.clone())
        }),
        "font-mono" => Some(quote! {
            .font_family(gpui_kit::component::Theme::global(cx).mono_font_family.clone())
        }),
        "font-serif" => Some(quote! { .font_family("Georgia") }),
        _ => None,
    }
}

fn parse_grid_class(class: &str) -> Option<TokenStream> {
    const GRID: &[(&str, &str, bool)] = &[
        ("grid-cols-", "grid_cols", false),
        ("grid-rows-", "grid_rows", false),
        ("col-span-", "col_span", false),
        ("row-span-", "row_span", false),
        ("col-start-", "col_start", true),
        ("col-end-", "col_end", true),
        ("row-start-", "row_start", true),
        ("row-end-", "row_end", true),
    ];
    let (value, method, signed) = GRID
        .iter()
        .find_map(|&(prefix, method, signed)| Some((class.strip_prefix(prefix)?, method, signed)))?;
    if matches!(value, "full" | "auto") {
        return None;
    }
    let method = syn::Ident::new(method, Span::call_site());
    let arg = if signed {
        value.parse::<i16>().ok().map(|n| quote! { #n })
    } else {
        value.parse::<u16>().ok().map(|n| quote! { #n })
    };
    Some(match arg {
        Some(arg) => quote! { .#method(#arg) },
        None => compile_error(format!("Invalid grid class `{class}`: expected a whole number.")),
    })
}

/// GPUI has no `justify-items`, so `place-items-*` aligns the cross axis only.
fn parse_place_class(class: &str) -> Option<TokenStream> {
    const ALIGN: &[&str] = &["start", "end", "center", "baseline", "stretch"];
    const CONTENT: &[&str] = &["start", "end", "center", "between", "around", "evenly", "stretch"];
    let (methods, values, value): (&[&str], _, _) =
        if let Some(value) = class.strip_prefix("place-items-") {
            (&["items"], ALIGN, value)
        } else if let Some(value) = class.strip_prefix("place-self-") {
            (&["self"], ALIGN, value)
        } else {
            let value = class.strip_prefix("place-content-")?;
            let methods: &[&str] = match value {
                "stretch" => &["content"],
                _ => &["content", "justify"],
            };
            (methods, CONTENT, value)
        };
    if !values.contains(&value) {
        return Some(compile_error(format!("Unsupported place class `{class}`.")));
    }
    let methods = methods
        .iter()
        .map(|method| syn::Ident::new(&format!("{method}_{value}"), Span::call_site()));
    Some(quote! { #(.#methods())* })
}

/// `grow-2` / `shrink-0`: any flex factor.
fn parse_flex_factor_class(class: &str) -> Option<TokenStream> {
    let (method, value) = match class.strip_prefix("grow-") {
        Some(value) => ("flex_grow", value),
        None => ("flex_shrink", class.strip_prefix("shrink-")?),
    };
    let value = parse_length_number(value).filter(|n| *n >= 0.0)?;
    let method = syn::Ident::new(method, Span::call_site());
    Some(quote! { .#method(#value) })
}

/// `overflow-visible` / `overflow-clip` (also `-x-` / `-y-`), which GPUI has no methods
/// for, and `overflow-auto`, which is GPUI's scroll.
fn parse_overflow_class(class: &str) -> Option<TokenStream> {
    let rest = class.strip_prefix("overflow-")?;
    let (axes, value): (&[&str], _) = match rest.split_once('-') {
        Some(("x", value)) => (&["x"], value),
        Some(("y", value)) => (&["y"], value),
        _ => (&["x", "y"], rest),
    };
    let variant = match value {
        "visible" => "Visible",
        "clip" => "Clip",
        "auto" => {
            let method = match axes {
                ["x"] => "overflow_x_scroll",
                ["y"] => "overflow_y_scroll",
                _ => "overflow_scroll",
            };
            let method = syn::Ident::new(method, Span::call_site());
            return Some(quote! { .#method() });
        }
        _ => return None,
    };
    let variant = syn::Ident::new(variant, Span::call_site());
    let axes = axes.iter().map(|axis| syn::Ident::new(axis, Span::call_site()));
    Some(wrap(quote! {
        #(gpui_kit::Styled::style(&mut __el).overflow.#axes = Some(gpui_kit::Overflow::#variant);)*
        __el
    }))
}

fn parse_scrollbar_class(class: &str) -> Option<TokenStream> {
    let width = match class.strip_prefix("scrollbar-")? {
        "none" => quote! { gpui_kit::px(0.0) },
        "thin" => quote! { gpui_kit::px(8.0) },
        value => {
            let inner = value.strip_prefix('[').and_then(|v| v.strip_suffix(']'));
            match inner.map(|inner| LengthKind::parse_arbitrary(class, inner)) {
                Some(Ok(LengthKind::Px(n))) => quote! { gpui_kit::px(#n) },
                Some(Ok(LengthKind::Rem(n))) => quote! { gpui_kit::rems(#n) },
                _ => {
                    return Some(compile_error(format!(
                        "Unsupported scrollbar class `{class}`. Expected scrollbar-none, \
                         scrollbar-thin, or a width like `scrollbar-[10px]`."
                    )));
                }
            }
        }
    };
    Some(quote! { .scrollbar_width(#width) })
}

/// Tailwind v4's radius scale; `s` / `e` / `ss` / `se` / `es` / `ee` read left-to-right.
fn parse_rounded_class(class: &str) -> Option<TokenStream> {
    let rest = class.strip_prefix("rounded")?;
    let rest = match rest.strip_prefix('-') {
        Some(rest) => rest,
        None if rest.is_empty() => rest,
        None => return None,
    };
    if rest.starts_with("theme") {
        return None;
    }
    let side = |side: &str| -> Option<&'static str> {
        Some(match side {
            "t" => "rounded_t",
            "b" => "rounded_b",
            "l" | "s" => "rounded_l",
            "r" | "e" => "rounded_r",
            "tl" | "ss" => "rounded_tl",
            "tr" | "se" => "rounded_tr",
            "bl" | "es" => "rounded_bl",
            "br" | "ee" => "rounded_br",
            _ => return None,
        })
    };
    let (method, size) = match rest.split_once('-') {
        Some((s, size)) if side(s).is_some() => (side(s)?, size),
        _ => match side(rest) {
            Some(method) => (method, ""),
            None => ("rounded", rest),
        },
    };
    let rems = |rems: f32| quote! { gpui_kit::rems(#rems) };
    let length = match size {
        "" | "sm" => rems(0.25),
        "xs" => rems(0.125),
        "md" => rems(0.375),
        "lg" => rems(0.5),
        "xl" => rems(0.75),
        "2xl" => rems(1.0),
        "3xl" => rems(1.5),
        "4xl" => rems(2.0),
        "none" => rems(0.0),
        "full" => quote! { gpui_kit::px(9999.0) },
        _ => {
            let inner = size.strip_prefix('[').and_then(|s| s.strip_suffix(']'));
            match inner.map(|inner| LengthKind::parse_arbitrary(class, inner)) {
                Some(Ok(LengthKind::Px(n))) => quote! { gpui_kit::px(#n) },
                Some(Ok(LengthKind::Rem(n))) => rems(n),
                _ => {
                    return Some(compile_error(format!(
                        "Unsupported radius class `{class}`. Expected none, xs, sm, md, lg, xl, \
                         2xl, 3xl, 4xl, full, or a length like `rounded-[6px]`."
                    )));
                }
            }
        }
    };
    // Through `Styled`: components such as `Button` have a `rounded` of their own.
    let method = syn::Ident::new(method, Span::call_site());
    Some(wrap(quote! { gpui_kit::Styled::#method(__el, #length) }))
}

/// `bg-linear-to-r from-sky-500 to-indigo-500/50`, with optional `from-10%` / `to-90%`.
/// GPUI gradients have two stops, so `via-*` isn't supported.
#[derive(Default)]
struct Gradient {
    angle: Option<f32>,
    from: Option<TokenStream>,
    to: Option<TokenStream>,
    from_at: Option<f32>,
    to_at: Option<f32>,
    errors: Vec<TokenStream>,
}

impl Gradient {
    fn take(&mut self, class: &str) -> bool {
        if let Some(direction) = class
            .strip_prefix("bg-linear-to-")
            .or_else(|| class.strip_prefix("bg-gradient-to-"))
        {
            let angle = match direction {
                "t" => 0.0,
                "tr" => 45.0,
                "r" => 90.0,
                "br" => 135.0,
                "b" => 180.0,
                "bl" => 225.0,
                "l" => 270.0,
                "tl" => 315.0,
                _ => {
                    self.errors.push(compile_error(format!(
                        "Unknown gradient direction in `{class}`. Expected t, tr, r, br, b, bl, l, or tl."
                    )));
                    return true;
                }
            };
            self.angle = Some(angle);
            return true;
        }
        if let Some(angle) = class.strip_prefix("bg-linear-") {
            let angle = angle
                .strip_prefix('[')
                .and_then(|a| a.strip_suffix("deg]"))
                .unwrap_or(angle);
            return match parse_length_number(angle) {
                Some(angle) => {
                    self.angle = Some(angle);
                    true
                }
                None => false,
            };
        }
        if class.starts_with("via-") {
            self.errors.push(compile_error(format!(
                "`{class}`: GPUI gradients have two stops, so only `from-*` and `to-*` are supported."
            )));
            return true;
        }
        let (stop, at, value) = if let Some(value) = class.strip_prefix("from-") {
            (&mut self.from, &mut self.from_at, value)
        } else if let Some(value) = class.strip_prefix("to-") {
            (&mut self.to, &mut self.to_at, value)
        } else {
            return false;
        };
        if let Some(percent) = value.strip_suffix('%').and_then(parse_length_number) {
            *at = Some(percent / 100.0);
            return true;
        }
        match color_tokens(value, class) {
            Some(Ok(color)) => *stop = Some(color),
            Some(Err(message)) => self.errors.push(compile_error(message)),
            None => return false,
        }
        true
    }

    fn finish(self) -> Vec<TokenStream> {
        let mut result = self.errors;
        let Some(angle) = self.angle else {
            if self.from.is_some() || self.to.is_some() {
                result.push(compile_error(
                    "`from-*` / `to-*` need a gradient direction, e.g. `bg-linear-to-r`.".into(),
                ));
            }
            return result;
        };
        let stop = |color: Option<TokenStream>, at: f32| {
            let color = color.unwrap_or_else(|| quote! { gpui_kit::transparent_black() });
            quote! { gpui_kit::linear_color_stop(#color, #at) }
        };
        let from = stop(self.from, self.from_at.unwrap_or(0.0));
        let to = stop(self.to, self.to_at.unwrap_or(1.0));
        result.push(quote! { .bg(gpui_kit::linear_gradient(#angle, #from, #to)) });
        result
    }
}

/// GPUI's `shadow_*` presets are Tailwind v4's.
const SHADOWS: &[&str] = &["2xs", "xs", "sm", "md", "lg", "xl", "2xl"];

/// GPUI has no inset presets, so these are Tailwind's: y offset, blur (px), black's alpha.
const INSET_SHADOWS: &[(&str, (f32, f32, f32))] = &[
    ("2xs", (1.0, 0.0, 0.05)),
    ("xs", (1.0, 1.0, 0.05)),
    ("sm", (2.0, 4.0, 0.05)),
];

/// `shadow-*`, `inset-shadow-*`, `ring-*` and `inset-ring-*` are all box shadows in GPUI:
/// the preset is GPUI's own method, then colors and extra layers go onto its list.
#[derive(Default)]
struct Shadows {
    preset: Option<String>,
    color: Option<TokenStream>,
    /// `Some(None)` is `inset-shadow-none`.
    inset: Option<Option<(f32, f32, f32)>>,
    inset_color: Option<TokenStream>,
    ring: Option<f32>,
    ring_color: Option<TokenStream>,
    inset_ring: Option<f32>,
    inset_ring_color: Option<TokenStream>,
    errors: Vec<TokenStream>,
}

impl Shadows {
    fn take(&mut self, class: &str) -> bool {
        match class {
            "shadow" => self.preset = Some("shadow_sm".into()),
            "shadow-none" => self.preset = Some("shadow_none".into()),
            "shadow-inner" => self.inset = Some(Some(INSET_SHADOWS[2].1)),
            "inset-shadow-none" => self.inset = Some(None),
            "ring" => self.ring = Some(1.0),
            "inset-ring" => self.inset_ring = Some(1.0),
            _ => return self.take_value(class),
        }
        true
    }

    fn take_value(&mut self, class: &str) -> bool {
        let (color, value) = if let Some(value) = class.strip_prefix("inset-shadow-") {
            if let Some((_, layer)) = INSET_SHADOWS.iter().find(|(name, _)| *name == value) {
                self.inset = Some(Some(*layer));
                return true;
            }
            (&mut self.inset_color, value)
        } else if let Some(value) = class.strip_prefix("shadow-") {
            if SHADOWS.contains(&value) {
                self.preset = Some(format!("shadow_{value}"));
                return true;
            }
            (&mut self.color, value)
        } else if let Some(value) = class.strip_prefix("inset-ring-") {
            if let Some(width) = ring_width(value) {
                self.inset_ring = Some(width);
                return true;
            }
            (&mut self.inset_ring_color, value)
        } else if let Some(value) = class.strip_prefix("ring-") {
            if let Some(width) = ring_width(value) {
                self.ring = Some(width);
                return true;
            }
            (&mut self.ring_color, value)
        } else {
            return false;
        };
        match color_tokens(value, class) {
            Some(Ok(tokens)) => *color = Some(tokens),
            Some(Err(message)) => self.errors.push(compile_error(message)),
            None => return false,
        }
        true
    }

    fn finish(self) -> Vec<TokenStream> {
        let mut result = self.errors;
        if let Some(preset) = &self.preset {
            let preset = syn::Ident::new(preset, Span::call_site());
            result.push(quote! { .#preset() });
        }

        let layer = |color: TokenStream, y: f32, blur: f32, spread: f32, inset: bool| {
            let inset = inset.then(|| quote! { .inset() });
            quote! {
                gpui_kit::BoxShadow::new(gpui_kit::px(0.0), gpui_kit::px(#y), gpui_kit::Hsla::from(#color))
                    .blur_radius(gpui_kit::px(#blur))
                    .spread_radius(gpui_kit::px(#spread))
                    #inset
            }
        };
        let theme_ring = || quote! { gpui_kit::component::Theme::global(cx).ring };
        let mut layers = Vec::new();
        if let Some(Some((y, blur, alpha))) = self.inset {
            let color = self
                .inset_color
                .unwrap_or_else(|| quote! { gpui_kit::hsla(0.0, 0.0, 0.0, #alpha) });
            layers.push(layer(color, y, blur, 0.0, true));
        }
        if let Some(width) = self.ring {
            layers.push(layer(self.ring_color.unwrap_or_else(theme_ring), 0.0, 0.0, width, false));
        }
        if let Some(width) = self.inset_ring {
            let color = self.inset_ring_color.unwrap_or_else(theme_ring);
            layers.push(layer(color, 0.0, 0.0, width, true));
        }

        let clear_inset = matches!(self.inset, Some(None))
            .then(|| quote! { __shadows.retain(|shadow| !shadow.inset); });
        let recolor = self.color.map(|color| {
            quote! {
                let __color = gpui_kit::Hsla::from(#color);
                for shadow in __shadows.iter_mut().filter(|shadow| !shadow.inset) {
                    shadow.color = __color;
                }
            }
        });
        if !layers.is_empty() || clear_inset.is_some() || recolor.is_some() {
            result.push(wrap(quote! {
                let __shadows = gpui_kit::Styled::style(&mut __el)
                    .box_shadow
                    .get_or_insert_with(Vec::new);
                #recolor
                #clear_inset
                __shadows.extend([#(#layers),*]);
                __el
            }));
        }
        result
    }
}

fn ring_width(value: &str) -> Option<f32> {
    if let Ok(width) = value.parse::<u16>() {
        return Some(width as f32);
    }
    value
        .strip_prefix('[')?
        .strip_suffix("px]")
        .and_then(parse_length_number)
}

fn parse_tailwind_alias_class(class: &str) -> Option<TokenStream> {
    match class {
        "flex-grow" | "grow" => Some(quote! { .flex_grow_1() }),
        "flex-shrink" | "shrink" => Some(quote! { .flex_shrink_1() }),
        "grow-0" => Some(quote! { .flex_grow_0() }),
        "shrink-0" => Some(quote! { .flex_shrink_0() }),
        "w-fit" => Some(quote! { .w_auto() }),
        "h-fit" => Some(quote! { .h_auto() }),
        "min-w-fit" => Some(quote! { .min_w_auto() }),
        "max-w-fit" => Some(quote! { .max_w_auto() }),
        "min-h-fit" => Some(quote! { .min_h_auto() }),
        "max-h-fit" => Some(quote! { .max_h_auto() }),
        _ => None,
    }
}

fn parse_font_weight_class(class: &str, mode: ClassMode) -> Option<TokenStream> {
    let weight = match class {
        "font-thin" => "THIN",
        "font-extralight" => "EXTRA_LIGHT",
        "font-light" => "LIGHT",
        "font-normal" => "NORMAL",
        "font-medium" => "MEDIUM",
        "font-semibold" => "SEMIBOLD",
        "font-bold" => "BOLD",
        "font-extrabold" => "EXTRA_BOLD",
        "font-black" => "BLACK",
        _ => {
            if mode.is_strict() && class.starts_with("font-") {
                let msg = format!(
                    "Unsupported font weight class `{class}`. Supported font weight classes: \
                     font-thin, font-extralight, font-light, font-normal, font-medium, \
                     font-semibold, font-bold, font-extrabold, font-black."
                );
                return Some(compile_error(msg));
            }
            return None;
        }
    };
    let weight_ident = syn::Ident::new(weight, Span::call_site());
    Some(quote! { .font_weight(FontWeight::#weight_ident) })
}

fn parse_object_fit_class(class: &str) -> Option<TokenStream> {
    let fit = match class {
        "object-cover" => "Cover",
        "object-contain" => "Contain",
        "object-fill" => "Fill",
        "object-scale-down" => "ScaleDown",
        "object-none" => "None",
        _ => return None,
    };
    let fit_ident = syn::Ident::new(fit, Span::call_site());
    Some(quote! { .object_fit(ObjectFit::#fit_ident) })
}

#[derive(Clone, Copy)]
pub(crate) enum LengthKind {
    Px(f32),
    Rem(f32),
    Relative(f32),
}

impl LengthKind {
    pub(crate) fn parse_arbitrary(class: &str, value: &str) -> Result<Self, String> {
        if let Some(raw) = value.strip_suffix("px") {
            return parse_length_number(raw)
                .map(Self::Px)
                .ok_or_else(|| invalid_length_value_message(class));
        }
        if let Some(raw) = value.strip_suffix("rem") {
            return parse_length_number(raw)
                .map(Self::Rem)
                .ok_or_else(|| invalid_length_value_message(class));
        }
        if let Some(raw) = value.strip_suffix('%') {
            return parse_length_number(raw)
                .map(|n| Self::Relative(n / 100.0))
                .ok_or_else(|| invalid_length_value_message(class));
        }

        Err(invalid_length_value_message(class))
    }

    pub(crate) fn parse_fraction(class: &str, value: &str) -> Result<Self, String> {
        let (numerator, denominator) = value.split_once('/').ok_or_else(|| {
            format!("Invalid fraction `{class}`: expected numerator/denominator.")
        })?;

        let numerator = parse_length_number(numerator)
            .ok_or_else(|| format!("Invalid fraction `{class}`: numerator must be a number."))?;
        let denominator = parse_length_number(denominator)
            .ok_or_else(|| format!("Invalid fraction `{class}`: denominator must be a number."))?;

        if denominator <= 0.0 {
            return Err(format!(
                "Invalid fraction `{class}`: denominator must be greater than 0."
            ));
        }

        Ok(Self::Relative(numerator / denominator))
    }
}

fn parse_arbitrary_length_class(class: &str) -> Option<TokenStream> {
    let (method, value, family) = split_length_class(class)?;
    if !value.starts_with('[') {
        return None;
    }

    let Some(inner) = value.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return Some(compile_error(format!(
            "Invalid length class `{class}`. Expected a numeric value with px, rem, or %, \
             for example `w-[280px]`."
        )));
    };

    let length = match LengthKind::parse_arbitrary(class, inner) {
        Ok(length) => length,
        Err(msg) => return Some(compile_error(msg)),
    };

    if matches!(length, LengthKind::Relative(_)) && !family.allows_percent() {
        return Some(compile_error(format!(
            "Invalid spacing class `{class}`. Percentage values are only supported for sizing \
             classes such as `w-*` and `h-*`."
        )));
    }

    Some(length_method_call(method, length))
}

fn parse_fraction_length_class(class: &str) -> Option<TokenStream> {
    let (method, value, family) = split_length_class(class)?;
    if !value.contains('/') {
        return None;
    }

    if !family.allows_fraction() {
        return Some(compile_error(format!(
            "Invalid fraction `{class}`: fractions are only supported for sizing classes."
        )));
    }

    match LengthKind::parse_fraction(class, value) {
        Ok(length) => Some(length_method_call(method, length)),
        Err(msg) => Some(compile_error(msg)),
    }
}

/// `mt-4`, `-mt-4`, and the keywords `px`, `full` and `auto`.
fn parse_numeric_length_class(class: &str) -> Option<TokenStream> {
    let (sign, unsigned) = match class.strip_prefix('-') {
        Some(unsigned) => (-1.0, unsigned),
        None => (1.0, class),
    };
    let (method, value, _) = split_length_class(unsigned)?;
    if value.starts_with('[') || value.contains('/') {
        return None;
    }
    let ident = syn::Ident::new(method, Span::call_site());
    match value {
        "px" => {
            let px = sign;
            Some(quote! { .#ident(gpui_kit::px(#px)) })
        }
        "full" if sign > 0.0 => Some(quote! { .#ident(gpui_kit::relative(1.0)) }),
        "auto" if sign > 0.0 => Some(quote! { .#ident(gpui_kit::auto()) }),
        _ => parse_length_number(value)
            .map(|value| length_method_call(method, LengthKind::Rem(sign * value * 0.25))),
    }
}

fn parse_direct_color_class(class: &str) -> Option<TokenStream> {
    if let Some(rest) = class.strip_prefix("text-") {
        return parse_color_with_method(rest, "text_color", class);
    }
    if let Some(rest) = class.strip_prefix("bg-") {
        return parse_color_with_method(rest, "bg", class);
    }
    if let Some(rest) = class.strip_prefix("border-")
        && !is_directional_border(rest)
    {
        return parse_color_with_method(rest, "border_color", class);
    }
    None
}

fn parse_direct_opacity_class(class: &str) -> Option<TokenStream> {
    let rest = class.strip_prefix("opacity-")?;
    let n = match rest.parse::<u8>() {
        Ok(n) if n <= 100 => n,
        _ => return Some(compile_error(invalid_opacity_value_message(class))),
    };
    let val = n as f32 / 100.0;
    Some(quote! { .opacity(#val) })
}

fn parse_direct_line_clamp_class(class: &str) -> Option<TokenStream> {
    let rest = class.strip_prefix("line-clamp-")?;
    let lines = rest.parse::<usize>().ok()?;
    Some(quote! { .line_clamp(#lines) })
}

fn parse_direct_directional_border_class(class: &str) -> Option<TokenStream> {
    let method = match class {
        "border-t" => "border_t_1",
        "border-b" => "border_b_1",
        "border-l" => "border_l_1",
        "border-r" => "border_r_1",
        "border-x" => "border_x_1",
        "border-y" => "border_y_1",
        _ => return None,
    };
    let ident = syn::Ident::new(method, Span::call_site());
    Some(quote! { .#ident() })
}

pub(crate) fn parse_length_number(raw: &str) -> Option<f32> {
    if raw.is_empty() {
        return None;
    }
    let value = raw.parse::<f32>().ok()?;
    value.is_finite().then_some(value)
}

pub(crate) fn invalid_length_value_message(class: &str) -> String {
    format!(
        "Invalid length class `{class}`. Expected a numeric value with px, rem, or %, \
         for example `w-[280px]`."
    )
}

fn length_method_call(method: &str, length: LengthKind) -> TokenStream {
    let method_ident = syn::Ident::new(method, Span::call_site());
    match length {
        LengthKind::Px(value) => quote! { .#method_ident(gpui_kit::px(#value)) },
        LengthKind::Rem(value) => quote! { .#method_ident(gpui_kit::rems(#value)) },
        LengthKind::Relative(value) => quote! { .#method_ident(gpui_kit::relative(#value)) },
    }
}

/// A compiler warning. Proc macros can't warn on stable, so this names a deprecated item
/// whose note is the message.
fn warning(message: String) -> TokenStream {
    wrap(quote! {
        {
            #[deprecated(note = #message)]
            #[allow(non_camel_case_types)]
            struct unsupported_class;
            let _ = unsupported_class;
        }
        __el
    })
}

fn compile_error(message: String) -> TokenStream {
    wrap(quote! { compile_error!(#message); __el })
}

/// Unified color parsing function (core deduplication logic)
///
/// Converts a color name or arbitrary hex value into a method call.
///
/// # Arguments
/// - `color`: Color string (e.g. "red_500", "[#ff0000]")
/// - `method`: Method name ("text_color", "bg", "border_color")
///
/// # Returns
/// - `Some(TokenStream)`: Successfully parsed, returning `.method(rgb(value))`
/// - `None`: Unable to parse color
fn parse_color_with_method(color: &str, method: &str, class: &str) -> Option<TokenStream> {
    let ident = syn::Ident::new(method, Span::call_site());
    Some(match color_tokens(color, class)? {
        Ok(color) => quote! { .#ident(#color) },
        Err(message) => compile_error(message),
    })
}

/// A color class's value — `red-500`, `theme-primary`, `[#0af]`, `transparent` — with an
/// optional `/50`, `/[0.5]` or `/[50%]` opacity, as an expression that converts into both
/// `Hsla` and `Fill`. `None` when it isn't a color at all.
fn color_tokens(color: &str, class: &str) -> Option<Result<TokenStream, String>> {
    let (color, alpha) = match color.rsplit_once('/') {
        Some((color, alpha)) => (color, Some(alpha)),
        None => (color, None),
    };

    // Dynamic theme color passthrough (e.g. `bg-theme-background`, `text-theme-foreground`, `bg-theme-muted`)
    let theme = color
        .strip_prefix("theme-")
        .or_else(|| color.strip_prefix("theme_"))
        .map(|field| field.replace('-', "_"))
        .map(|field| match field.strip_suffix("_background") {
            Some(base) if !base.is_empty() => base.to_string(),
            _ => field,
        })
        .filter(|field| {
            !field.is_empty() && field.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
    let base = if let Some(field) = theme {
        let field = syn::Ident::new(&field, Span::call_site());
        Some(quote! { gpui_kit::component::Theme::global(cx).#field })
    } else if color == "transparent" {
        Some(quote! { gpui_kit::transparent_black() })
    } else {
        None
    };

    let value = if base.is_some() {
        None
    } else {
        match lookup_color_key(color)
            .map(ColorValue::Rgb)
            .or_else(|| parse_arbitrary_color_value(color))
        {
            Some(value) => Some(value),
            None if color.starts_with('[') => return Some(Err(invalid_color_value_message(class))),
            None => return None,
        }
    };

    let alpha = match alpha.map(parse_alpha) {
        Some(Some(alpha)) => Some(alpha),
        Some(None) => {
            return Some(Err(format!(
                "Invalid opacity in `{class}`. Expected `/0` through `/100`, `/[0.5]`, or `/[50%]`."
            )));
        }
        None => None,
    };

    Some(Ok(match (base, value, alpha) {
        (Some(base), _, Some(alpha)) => quote! { #base.opacity(#alpha) },
        (Some(base), _, None) => base,
        (None, Some(ColorValue::Rgb(hex)), None) => quote! { gpui_kit::rgb(#hex) },
        (None, Some(ColorValue::Rgb(hex)), Some(alpha)) => {
            let rgba = (hex << 8) | (alpha * 255.0).round() as u32;
            quote! { gpui_kit::rgba(#rgba) }
        }
        (None, Some(ColorValue::Rgba(hex)), alpha) => {
            let a = ((hex & 0xff) as f32 * alpha.unwrap_or(1.0)).round() as u32;
            let rgba = (hex & !0xff) | a;
            quote! { gpui_kit::rgba(#rgba) }
        }
        (None, None, _) => unreachable!(),
    }))
}

/// `50` → 0.5, `[0.5]` → 0.5, `[50%]` → 0.5.
fn parse_alpha(alpha: &str) -> Option<f32> {
    let value = match alpha.strip_prefix('[').and_then(|a| a.strip_suffix(']')) {
        Some(inner) => match inner.strip_suffix('%') {
            Some(percent) => parse_length_number(percent)? / 100.0,
            None => parse_length_number(inner)?,
        },
        None => alpha.parse::<u8>().ok()? as f32 / 100.0,
    };
    (0.0..=1.0).contains(&value).then_some(value)
}

fn lookup_color_key(color: &str) -> Option<u32> {
    if let Some(hex) = lookup_color(color) {
        return Some(hex);
    }
    if color.contains('-') {
        let normalized = color.replace('-', "_");
        lookup_color(&normalized)
    } else {
        None
    }
}

/// Determines whether the part after `border_` belongs to a directional border class (rather than a color class)
///
/// Directional borders (fall through to default method call):
/// - Plain direction: `border-t` → rest = `"t"` (len == 1)
/// - Direction + value: `border-t-2` → rest = `"t-2"` / `"t_2"` (first byte is direction)
///
/// Color classes (should generate `.border_color(rgb(...))`):
/// - `border-red-500` → rest = `"red_500"` (first byte `r` is in directional set,
///   but second byte `e` != `_`, so it is classified as a color class)
fn is_directional_border(rest: &str) -> bool {
    let bytes = rest.as_bytes();
    matches!(bytes.first(), Some(b't' | b'b' | b'l' | b'r' | b'x' | b'y'))
        && (rest.len() == 1 || matches!(bytes.get(1), Some(b'-' | b'_')))
}

fn lookup_directional_border_method(class: &str) -> Option<&'static str> {
    match class {
        "border_t" => Some("border_t_1"),
        "border_b" => Some("border_b_1"),
        "border_l" => Some("border_l_1"),
        "border_r" => Some("border_r_1"),
        "border_x" => Some("border_x_1"),
        "border_y" => Some("border_y_1"),
        _ => None,
    }
}

/// Parses arbitrary hex color values: `[#rrggbb]` or `[#rgb]`
///
/// Input has already undergone `-` → `_` replacement, but `[#...]` contains no `-`, so it stays intact.
/// Returns the parsed u32 color value.
#[derive(Clone, Copy)]
enum ColorValue {
    Rgb(u32),
    Rgba(u32),
}

fn parse_arbitrary_color_value(s: &str) -> Option<ColorValue> {
    parse_arbitrary_hex(s).or_else(|| parse_arbitrary_color_function(s))
}

fn parse_arbitrary_hex(s: &str) -> Option<ColorValue> {
    // Match [#rrggbb], [#rrggbbaa], [#rgb], or [#rgba]
    let inner = s.strip_prefix("[#")?.strip_suffix(']')?;
    match inner.len() {
        8 => u32::from_str_radix(inner, 16).ok().map(ColorValue::Rgba),
        6 => u32::from_str_radix(inner, 16).ok().map(ColorValue::Rgb),
        4 => {
            let b = inner.as_bytes();
            let d = |c: u8| -> Option<u32> { (c as char).to_digit(16) };
            let r = d(b[0])?;
            let g = d(b[1])?;
            let bl = d(b[2])?;
            let a = d(b[3])?;
            let expand = |n: u32| (n << 4) | n;
            Some(ColorValue::Rgba(
                expand(r) << 24 | expand(g) << 16 | expand(bl) << 8 | expand(a),
            ))
        }
        3 => {
            // Expand 3-digit hex to 6 digits: #abc → #aabbcc
            // Implemented with bitwise operations for zero allocation, avoiding String heap allocation
            let b = inner.as_bytes();
            let d = |c: u8| -> Option<u32> { (c as char).to_digit(16) };
            let r = d(b[0])?;
            let g = d(b[1])?;
            let bl = d(b[2])?;
            // Duplicate each 4-bit digit to upper and lower nibbles: 0xA → 0xAA
            Some(ColorValue::Rgb(
                r << 20 | r << 16 | g << 12 | g << 8 | bl << 4 | bl,
            ))
        }
        _ => None,
    }
}

fn parse_arbitrary_color_function(s: &str) -> Option<ColorValue> {
    let inner = s.strip_prefix('[')?.strip_suffix(']')?;

    if let Some(args) = inner.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let [r, g, b] = parse_rgb_args(args)?;
        return Some(ColorValue::Rgb(
            ((r as u32) << 16) | ((g as u32) << 8) | b as u32,
        ));
    }

    if let Some(args) = inner
        .strip_prefix("rgba(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let mut parts = args.split(',');
        let r = parse_u8_component(parts.next()?)?;
        let g = parse_u8_component(parts.next()?)?;
        let b = parse_u8_component(parts.next()?)?;
        let a = parse_alpha_component(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        return Some(ColorValue::Rgba(
            ((r as u32) << 24) | ((g as u32) << 16) | ((b as u32) << 8) | a as u32,
        ));
    }

    None
}

fn parse_rgb_args(args: &str) -> Option<[u8; 3]> {
    let mut parts = args.split(',');
    let r = parse_u8_component(parts.next()?)?;
    let g = parse_u8_component(parts.next()?)?;
    let b = parse_u8_component(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some([r, g, b])
}

fn parse_u8_component(raw: &str) -> Option<u8> {
    raw.trim().parse::<u8>().ok()
}

fn parse_alpha_component(raw: &str) -> Option<u8> {
    let value = raw.trim().parse::<f32>().ok()?;
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return None;
    }
    Some((value * 255.0).round() as u8)
}

fn invalid_color_value_message(class: &str) -> String {
    format!(
        "Invalid color class `{class}`. Expected #rgb, #rrggbb, #rrggbbaa, rgb(r,g,b), or rgba(r,g,b,a)."
    )
}

fn invalid_opacity_value_message(class: &str) -> String {
    format!("Invalid opacity class `{class}`. Expected `opacity-0` through `opacity-100`.")
}

fn unsupported_class_message(class: &str, method: &str) -> String {
    if method.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        format!(
            "Unknown class `{class}`. If it's a GPUI style method, use it as an attribute \
             instead: `<div {method} />`."
        )
    } else {
        format!("Unknown class `{class}`.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_arbitrary_length_reports_compile_error() {
        let tokens = parse_single_class_with_mode("w-[280px", ClassMode::Permissive).to_string();

        assert!(tokens.contains("Invalid length class `w-[280px`"));
        assert!(tokens.contains("compile_error"));
    }

    #[test]
    fn fraction_parser_reports_each_malformed_component() {
        let cases = [
            (
                "12",
                "Invalid fraction `w-12`: expected numerator/denominator.",
            ),
            (
                "many/2",
                "Invalid fraction `w-many/2`: numerator must be a number.",
            ),
            (
                "1/many",
                "Invalid fraction `w-1/many`: denominator must be a number.",
            ),
        ];

        for (value, expected) in cases {
            let class = format!("w-{value}");
            assert_eq!(
                LengthKind::parse_fraction(&class, value).err().as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn fraction_spacing_reports_unsupported_family() {
        let tokens = parse_single_class_with_mode("p-1/2", ClassMode::Permissive).to_string();

        assert!(tokens.contains("fractions are only supported for sizing classes"));
        assert!(tokens.contains("compile_error"));
    }

    #[test]
    fn length_number_rejects_empty_and_non_finite_values() {
        assert_eq!(parse_length_number(""), None);
        assert_eq!(parse_length_number("NaN"), None);
        assert_eq!(parse_length_number("inf"), None);
        assert_eq!(parse_length_number("-inf"), None);
    }
}

fn parse_arbitrary_bracket_length_class(class: &str) -> Option<TokenStream> {
    if let Some(rest) = class.strip_prefix("text-[")
        && let Some(inner) = rest.strip_suffix(']')
    {
        if let Some(num) = inner.strip_suffix("px")
            && let Ok(v) = num.parse::<f32>()
        {
            return Some(quote! { .text_size(gpui_kit::px(#v)) });
        } else if let Some(num) = inner.strip_suffix("rem")
            && let Ok(v) = num.parse::<f32>()
        {
            return Some(quote! { .text_size(gpui_kit::rems(#v)) });
        }
    }

    None
}


