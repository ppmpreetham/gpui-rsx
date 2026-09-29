use proc_macro2::TokenStream;
use quote::quote;
use syn::spanned::Spanned;
use crate::parser::{RsxAttribute, RsxElement, RsxNode};
use crate::codegen::{
    attribute::{generate_attr_methods_with_mode, AttrHints},
    class::ClassMode,
    element::generate_node_checked,
};

fn as_element<'a>(node: &'a RsxNode, tag: &str) -> Option<&'a RsxElement> {
    let RsxNode::Element(el) = node else { return None; };
    (el.name.to_string() == tag).then_some(el)
}

fn get_string_literal(node: &RsxNode) -> Option<String> {
    let RsxNode::Expr(syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. })) = node else { return None; };
    Some(s.value())
}

fn get_attr_value<'a>(attrs: &'a [RsxAttribute], target: &str) -> Option<&'a syn::Expr> {
    attrs.iter().find_map(|attr| {
        let RsxAttribute::Value { name, value } = attr else { return None; };
        (name.to_string() == target).then_some(value)
    })
}

pub fn parse_data_table(element: &RsxElement) -> Result<TokenStream, TokenStream> {
    let mut columns = Vec::new();
    let mut group_headers = Vec::new();

    if let Some(thead_el) = element.children.iter().find_map(|c| as_element(c, "thead")) {
        let tr_rows: Vec<_> = thead_el.children.iter().filter_map(|c| as_element(c, "tr")).collect();

        if let Some((&main_tr, group_trs)) = tr_rows.split_last() {
            group_headers = group_trs.iter().map(|&group_tr| {
                let row_groups: Vec<_> = group_tr.children.iter().filter_map(|th_node| {
                    let th = as_element(th_node, "th")?;
                    let label = th.children.iter().find_map(get_string_literal).unwrap_or_default();
                    let span = get_attr_value(&th.attributes, "colspan")
                        .map_or_else(|| quote! { 1 }, |val| quote! { #val });

                    Some(quote! { gpui_kit::component::table::ColumnGroup::new(#label, #span) })
                }).collect();

                quote! { vec![ #( #row_groups ),* ] }
            }).collect();

            columns = main_tr.children.iter().filter_map(|th_node| {
                let th = as_element(th_node, "th")?;
                Some((|| -> Result<TokenStream, TokenStream> {
                    let id_attr = get_attr_value(&th.attributes, "id").ok_or_else(|| {
                        syn::Error::new(th.name.span(), "<th> inside <DataTable> must have an `id` attribute").to_compile_error()
                    })?;

                    let name_text = th.children.iter().find_map(get_string_literal).unwrap_or_default();
                    let col_methods = th.attributes.iter().filter_map(|attr| match attr {
                        RsxAttribute::Value { name, value } if name.to_string() != "id" => Some(quote! { .#name(#value) }),
                        RsxAttribute::Flag(name) => Some(quote! { .#name() }),
                        _ => None,
                    });

                    Ok(quote! {
                        gpui_kit::component::table::Column::new(#id_attr, #name_text)
                        #( #col_methods )*
                    })
                })())
            }).collect::<Result<Vec<_>, _>>()?;
        }
    }

    let tbody_el = element.children.iter().find_map(|c| as_element(c, "tbody"));
    let items = tbody_el
        .and_then(|el| get_attr_value(&el.attributes, "items"))
        .ok_or_else(|| syn::Error::new(element.name.span(), "<tbody> requires `items` attribute").to_compile_error())?;

    let mut row_ident = quote! { _item };
    let mut tr_delegate_methods = Vec::new();
    let mut match_arms = Vec::new();

    if let Some(tbody_el) = tbody_el {
        let closure = tbody_el.children.iter().find_map(|c| {
            let RsxNode::Expr(syn::Expr::Closure(c)) = c else { return None; };
            Some(c)
        });

        if let Some(closure) = closure
            && let syn::Expr::Macro(expr_mac) = &*closure.body
            && expr_mac.mac.path.segments.last().is_some_and(|s| s.ident == "view")
        {
            if let Some(arg) = closure.inputs.first() {
                row_ident = quote! { #arg };
            }

            let tr_el = syn::parse2::<RsxElement>(expr_mac.mac.tokens.clone())
                .map_err(|e| syn::Error::new(expr_mac.span(), format!("Failed to parse inner view! TR: {}", e)).to_compile_error())?;

            if tr_el.name.to_string() == "tr" {
                // Restored: Common td methods hook
                let common_td_methods = Vec::<TokenStream>::new();

                let tr_methods = tr_el.attributes.iter().fold(Vec::new(), |mut methods, attr| {
                    if let RsxAttribute::Value { name, value } = attr
                        && matches!(name.to_string().as_str(), "on_context_menu" | "on_click" | "on_double_click")
                    {
                        tr_delegate_methods.push(quote! { .#name(move |_, _| { #value() }) });
                        return methods;
                    }

                    let name_str = match attr {
                        RsxAttribute::Value { name, .. } | RsxAttribute::Flag(name) => name.to_string(),
                        RsxAttribute::StateClass { method, .. } => method.to_string(),
                        RsxAttribute::When { .. } | RsxAttribute::WhenSome { .. } | RsxAttribute::WhenClass { .. } => String::new(),
                    };

                    let static_class = if name_str == "class"
                        && let RsxAttribute::Value { value: syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(lit_str), .. }), .. } = attr
                    {
                        Some(lit_str.value())
                    } else { None };

                    generate_attr_methods_with_mode(
                        attr,
                        AttrHints { name: Some(&name_str), static_class: static_class.as_deref() },
                        &mut methods,
                        ClassMode::Permissive,
                    );

                    methods
                });

                if !tr_methods.is_empty() {
                    tr_delegate_methods.push(quote! {
                        .render_row(move |row_ix, #row_ident, _window, _cx| {
                            use gpui_kit::IntoElement;
                            gpui_kit::div()
                                .id(row_ix)
                                #( #tr_methods )*
                                .child(gpui_kit::div().absolute().inset_0() #( #tr_methods )*)
                        })
                    });
                }

                match_arms = tr_el.children.iter().filter_map(|td_node| {
                    let td = as_element(td_node, "td")?;
                    Some((|| -> Result<TokenStream, TokenStream> {
                        let td_id = get_attr_value(&td.attributes, "id").ok_or_else(|| {
                            syn::Error::new(td.name.span(), "<td> inside <tbody items> must have an `id` attribute").to_compile_error()
                        })?;

                        let compiled_children = td.children.iter()
                            .map(|node| generate_node_checked(node, false, ClassMode::Permissive, &[]))
                            .collect::<Result<Vec<_>, _>>()?;

                        let td_methods = td.attributes.iter().fold(Vec::new(), |mut methods, attr| {
                            match attr {
                                RsxAttribute::Value { name, value } if name.to_string() != "id" => {
                                    let name_str = name.to_string();
                                    let static_class = if name_str == "class"
                                        && let syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(lit_str), .. }) = value
                                    {
                                        Some(lit_str.value())
                                    } else { None };

                                    generate_attr_methods_with_mode(
                                        attr,
                                        AttrHints { name: Some(&name_str), static_class: static_class.as_deref() },
                                        &mut methods,
                                        ClassMode::Permissive
                                    );
                                }
                                RsxAttribute::Flag(name) => methods.push(quote! { .#name() }),
                                _ => {}
                            }
                            methods
                        });

                        // Restored: #( #common_td_methods )* interpolation
                        let cell_content = if compiled_children.len() == 1 {
                            let child = &compiled_children[0];
                            quote! { gpui_kit::component::table::TableCell::new().p_0().h_full() #( #common_td_methods )* #( #td_methods )* .child(#child) }
                        } else {
                            quote! { gpui_kit::component::table::TableCell::new() #( #common_td_methods )* #( #td_methods )* #( .child(#compiled_children) )* }
                        };

                        Ok(quote! { #td_id => { #cell_content.into_any_element() } })
                    })())
                }).collect::<Result<Vec<_>, _>>()?;
            }
        }
    }

    let delegate_attrs = ["on_sort", "on_context_menu", "on_lazy_load", "group_headers"];
    let state_attrs = ["row_selectable", "cell_selectable", "col_selectable", "loop_selection", "row_header", "sortable"];

    let (delegate_methods, state_methods, table_methods) = element.attributes.iter().fold(
        (tr_delegate_methods, Vec::new(), Vec::new()),
        |(mut del, mut state, mut tab), attr| {
            match attr {
                RsxAttribute::Value { name, value } => {
                    let name_str = name.to_string();
                    if delegate_attrs.contains(&name_str.as_str()) {
                        del.push(quote! { .#name(#value) });
                    } else if state_attrs.contains(&name_str.as_str()) {
                        state.push(quote! { .#name(#value) });
                    } else {
                        let mapped_event = match name_str.as_str() {
                            "on_select_row" => Some(quote! { .on_select_row(#value) }),
                            "on_double_click_row" => Some(quote! { .on_double_click_row(#value) }),
                            "on_right_click_row" => Some(quote! { .on_right_click_row(#value) }),
                            "on_select_cell" => Some(quote! { .on_select_cell(#value) }),
                            "on_double_click_cell" => Some(quote! { .on_double_click_cell(#value) }),
                            "on_right_click_cell" => Some(quote! { .on_right_click_cell(#value) }),
                            "on_select_column" => Some(quote! { .on_select_column(#value) }),
                            "on_move_column" => Some(quote! { .on_move_column(#value) }),
                            "on_column_widths_changed" => Some(quote! { .on_column_widths_changed(#value) }),
                            _ => None,
                        };
                        if let Some(event) = mapped_event {
                            tab.push(event);
                        } else {
                            tab.push(quote! { .#name(#value) });
                        }
                    }
                },
                RsxAttribute::Flag(name) => {
                    let name_str = name.to_string();
                    if delegate_attrs.contains(&name_str.as_str()) {
                        del.push(quote! { .#name(true) });
                    } else if state_attrs.contains(&name_str.as_str()) {
                        state.push(quote! { .#name(true) });
                    } else {
                        tab.push(quote! { .#name(true) });
                    }
                },
                _ => {}
            }
            (del, state, tab)
        }
    );

    let group_headers_method = (!group_headers.is_empty())
        .then(|| quote! { .group_headers(vec![ #( #group_headers ),* ]) });

    // 4. Output final AST
    Ok(quote! {
        {
            let _delegate = zopra::components::declarative_table::DeclarativeTableDelegate::new(
                #items,
                vec![ #( #columns ),* ],
                |#row_ident, col_id, _window, _cx| {
                    use gpui_kit::IntoElement;
                    match col_id {
                        #( #match_arms )*
                        _ => gpui_kit::component::table::TableCell::new().into_any_element(),
                    }
                }
            )
            #group_headers_method
            #( #delegate_methods )*;

            let _state = zopra::hooks::use_table_with(
                || _delegate,
                |state| state #( #state_methods )*,
                window,
                cx
            );

            gpui_kit::component::table::DataTable::new(&_state)
            #( #table_methods )*
        }
    })
}
