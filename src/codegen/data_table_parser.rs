use proc_macro2::TokenStream;
use quote::quote;
use syn::Expr;

use crate::parser::{RsxAttribute, RsxElement};

pub fn parse_data_table(element: &RsxElement) -> Result<TokenStream, TokenStream> {
    let mut columns = Vec::new();
    let mut match_arms = Vec::new();
    let mut tr_delegate_methods = Vec::new();
    let mut sorters = Vec::new();
    let mut group_headers = Vec::new();
    
    let mut tbody_el = None;
    let mut items_expr = quote! { zopra::components::declarative_table::Rows::Owned(::std::vec::Vec::new()) };
    
    if let Some(rows_attr) = element.attributes.iter().find(|a| matches!(a, RsxAttribute::Value { name, .. } if name == "rows")) {
        if let RsxAttribute::Value { value, .. } = rows_attr {
            items_expr = quote! { zopra::components::declarative_table::Rows::from(#value) };
        }
    }

    for child in &element.children {
        match child {
            crate::parser::RsxNode::Element(el) => {
                let tag = el.name.to_string();
                if tag == "Col" {
                    parse_col(el, &mut columns, &mut match_arms, &mut sorters);
                } else if tag == "Group" {
                    let mut cols = Vec::new();
                    let title = el.attributes.iter().find_map(|a| {
                        if let RsxAttribute::Value { name, value } = a {
                            if name == "title" { Some(value.clone()) } else { None }
                        } else { None }
                    });
                    
                    for sub in &el.children {
                        if let crate::parser::RsxNode::Element(sub_el) = sub {
                            if sub_el.name.to_string() == "Col" {
                                parse_col(sub_el, &mut cols, &mut match_arms, &mut sorters);
                            }
                        }
                    }
                    if !cols.is_empty() {
                        let width = cols.len();
                        columns.extend(cols);
                        if let Some(t) = title {
                            group_headers.push(quote! { gpui_kit::component::table::ColumnGroup::new(#t, #width) });
                        }
                    }
                } else if tag == "tbody" {
                    tbody_el = Some(el);
                    if let Some(RsxAttribute::Value { value, .. }) = el.attributes.iter().find(|a| matches!(a, RsxAttribute::Value { name, .. } if name == "r")) {
                        tr_delegate_methods.push(quote! { .render_row(#value) });
                    }
                    if let Some(RsxAttribute::Value { value, .. }) = el.attributes.iter().find(|a| matches!(a, RsxAttribute::Value { name, .. } if name == "each")) {
                        items_expr = quote! { zopra::components::declarative_table::Rows::from(#value) };
                    }
                }
            },
            _ => {}
        }
    }

    let delegate_attrs = ["on_sort", "on_context_menu", "on_lazy_load", "group_headers", "rows_count", "data_offset", "on_visible_rows_changed"];
    let dynamic_attrs: [&str; 0] = [];
    let state_attrs = ["row_selectable", "cell_selectable", "col_selectable", "loop_selection", "row_header", "sortable"];
    let skip_attrs: &[&str] = if tbody_el.is_some() { &[] } else { &["rows"] };

    let (delegate_methods, state_methods, table_methods, event_handlers, dynamic_updates) = element.attributes.iter().fold(
        (tr_delegate_methods, Vec::new(), Vec::new(), Vec::new(), Vec::new()),
        |(mut del, mut state, mut tab, mut events, mut dyn_upd), attr| {
            match attr {
                RsxAttribute::Value { name, value } => {
                    let name_str = name.to_string();
                    if skip_attrs.contains(&name_str.as_str()) {
                    } else if delegate_attrs.contains(&name_str.as_str()) {
                        del.push(quote! { .#name(#value) });
                    } else if dynamic_attrs.contains(&name_str.as_str()) {
                        del.push(quote! { .#name(#value) });
                        dyn_upd.push(quote! { __d.#name(#value); });
                    } else if state_attrs.contains(&name_str.as_str()) {
                        state.push(quote! { .#name(#value) });
                    } else {
                        let mapped_event = match name_str.as_str() {
                            "on_select_row" => Some(quote! {
                                let __zopra_on_select_row: Box<dyn Fn(usize, &mut gpui_kit::App)> = Box::new(#value);
                                zopra::hooks::use_event(&_state, move |event, cx| {
                                    if let gpui_kit::component::table::TableEvent::SelectRow(ix) = event {
                                        __zopra_on_select_row(*ix, cx);
                                    }
                                }, window, cx);
                            }),
                            "on_double_click_row" => Some(quote! { .on_double_click_row(#value) }),
                            "on_right_click_row" => Some(quote! { .on_right_click_row(#value) }),
                            "on_visible_rows_changed" => Some(quote! { .on_visible_rows_changed(#value) }),
                            _ => None,
                        };
                        if let Some(event_expr) = mapped_event {
                            events.push(event_expr);
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
            (del, state, tab, events, dyn_upd)
        }
    );

    let group_headers_method = (!group_headers.is_empty())
        .then(|| quote! { .group_headers(vec![ #( #group_headers ),* ]) });

    // built before the rows move into the delegate; `col_sorter` reads them only for type inference
    let has_sorters = sorters.iter().any(|s| s.to_string() != "None");
    let sorters_stmt = has_sorters.then(|| quote! { let __zopra_sorters = vec![ #( #sorters ),* ]; });
    let sorters_method = has_sorters.then(|| quote! { .sorters(__zopra_sorters) });

    let row_ident = quote! { __row_item };
    let has_dynamic_updates = !dynamic_updates.is_empty();

    // the rows move into the delegate on the first render, and into `update_data` after that
    Ok(quote! {
        {
            let mut __zopra_rows = ::std::option::Option::Some(#items_expr);
            let _state = zopra::__private::use_table_with(
                || {
                    let __zopra_rows = __zopra_rows.take().expect("rows are taken once");
                    #sorters_stmt
                    let _delegate = zopra::components::declarative_table::DeclarativeTableDelegate::new(
                        __zopra_rows,
                        vec![ #( #columns ),* ],
                        move |#row_ident, col_id| {
                            use gpui_kit::IntoElement;
                            match col_id {
                                #( #match_arms )*
                                _ => gpui_kit::component::table::TableCell::new().into_any_element(),
                            }
                        }
                    )
                    #group_headers_method
                    #sorters_method
                    #( #delegate_methods )*;
                    _delegate
                },
                |state| state #( #state_methods )*,
                window,
                cx
            );

            if let ::std::option::Option::Some(__zopra_rows) = __zopra_rows {
                _state.update(cx, |state, cx| {
                    let __d = state.delegate_mut();
                    let changed = __d.update_data(__zopra_rows, cx);
                    #( #dynamic_updates )*
                    if changed || #has_dynamic_updates {
                        cx.notify();
                    }
                });
            }

            #( #event_handlers )*

            gpui_kit::component::table::DataTable::new(&_state)
            #( #table_methods )*
        }
    })
}

fn parse_col(
    el: &RsxElement, 
    columns: &mut Vec<TokenStream>, 
    match_arms: &mut Vec<TokenStream>,
    sorters: &mut Vec<TokenStream>
) {
    let mut key = String::new();
    let mut title = quote! { "" };
    let mut accessor = quote! { |_row| gpui_kit::div() };
    let mut col_methods = Vec::new();
    let mut sortable = false;

    for attr in &el.attributes {
        match attr {
            RsxAttribute::Value { name, value } => {
                let name_str = name.to_string();
                match name_str.as_str() {
                    "id" => {
                        if let Expr::Lit(expr_lit) = value {
                            if let syn::Lit::Str(lit_str) = &expr_lit.lit {
                                key = lit_str.value();
                            }
                        }
                    },
                    "title" => title = quote! { #value },
                    "r" => accessor = quote! { #value },
                    "width" => col_methods.push(quote! { .width(gpui_kit::px(#value as f32)) }),
                    _ => col_methods.push(quote! { .#name(#value) }),
                }
            },
            RsxAttribute::Flag(name) => {
                let name_str = name.to_string();
                if name_str == "sortable" {
                    sortable = true;
                } else {
                    col_methods.push(quote! { .#name() });
                }
            },
            _ => {}
        }
    }

    let col = quote! { gpui_kit::component::table::Column::new(#key, #title) #( #col_methods )* };
    columns.push(col);

    let row_ident = quote! { __row_item };
    
    let cell = quote! { {
        let __zopra_cell_val = zopra::components::declarative_table::cell_of(#accessor, #row_ident);
        gpui_kit::component::table::TableCell::new().p_0().h_full().child(__zopra_cell_val)
    } };
    match_arms.push(quote! { #key => { #cell.into_any_element() } });

    let sorter_expr = if sortable {
        quote! { Some(zopra::components::declarative_table::col_sorter(#accessor, __zopra_rows.probe())) }
    } else {
        quote! { None }
    };
    sorters.push(sorter_expr);
}




