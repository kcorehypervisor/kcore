//! Shared list-table pager: prev/next links plus a per-page dropdown.

use crate::format::{
    normalize_page_size, parse_page, query_href, PageView, DEFAULT_PAGE_SIZE, PAGE_SIZE_OPTIONS,
};
use leptos::prelude::*;
use leptos_router::params::ParamsMap;

/// `(page, per_page)` from the standard query keys.
pub fn list_paging(qmap: &ParamsMap) -> (u32, usize) {
    list_paging_named(qmap, "page")
}

/// `(page, per_page)` with a custom page query key (for multi-list pages).
pub fn list_paging_named(qmap: &ParamsMap, page_param: &str) -> (u32, usize) {
    let page = parse_page(qmap.get_str(page_param));
    let per_page = qmap
        .get_str("per_page")
        .and_then(|s| s.parse::<u32>().ok())
        .map(normalize_page_size)
        .unwrap_or(DEFAULT_PAGE_SIZE);
    (page, per_page)
}

/// Read an optional string query value (empty when absent).
pub fn query_string(qmap: &ParamsMap, key: &str) -> String {
    qmap.get(key).unwrap_or_default()
}

/// Standard pager for `page` / `per_page` query keys.
pub fn list_pager(
    path: &str,
    page: u32,
    page_size: usize,
    total: usize,
    item_label: &str,
    extra: &[(String, String)],
) -> AnyView {
    list_pager_named(path, "page", page, page_size, total, item_label, extra)
}

/// Pager with a custom page query parameter name.
pub fn list_pager_named(
    path: &str,
    page_param: &str,
    page: u32,
    page_size: usize,
    total: usize,
    item_label: &str,
    extra: &[(String, String)],
) -> AnyView {
    let page_size = normalize_page_size(page_size as u32);
    let page = page.max(1);
    let total_pages = if total == 0 {
        1
    } else {
        total.div_ceil(page_size) as u32
    };
    let has_prev = page > 1;
    let has_next = (page as usize).saturating_mul(page_size) < total;

    let mut link_pairs: Vec<(String, String)> = extra.to_vec();
    link_pairs.push(("per_page".into(), page_size.to_string()));

    let prev = page.saturating_sub(1);
    let next = page.saturating_add(1);
    let mut prev_pairs = link_pairs.clone();
    prev_pairs.push((page_param.to_string(), prev.to_string()));
    let mut next_pairs = link_pairs;
    next_pairs.push((page_param.to_string(), next.to_string()));

    let prev_refs: Vec<(&str, String)> = prev_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.clone()))
        .collect();
    let next_refs: Vec<(&str, String)> = next_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.clone()))
        .collect();

    let prev_href = if has_prev {
        query_href(path, &prev_refs)
    } else {
        String::new()
    };
    let next_href = if has_next {
        query_href(path, &next_refs)
    } else {
        String::new()
    };

    let summary = format!("Page {page} of {total_pages} — {total} {item_label}");
    let path_owned = path.to_string();
    let page_param_owned = page_param.to_string();
    let extras_owned: Vec<(String, String)> = extra.to_vec();
    let page_size_u = page_size;

    let prev_link = if has_prev {
        view! { <a href=prev_href>"← Previous"</a> }.into_any()
    } else {
        view! { <span></span> }.into_any()
    };
    let next_link = if has_next {
        view! { <a href=next_href>"Next →"</a> }.into_any()
    } else {
        view! { <span></span> }.into_any()
    };

    view! {
        <div class="pager">
            <div class="pager-nav">
                {prev_link}
                <span class="muted">{summary}</span>
                {next_link}
            </div>
            <form class="pager-size" method="get" action=path_owned>
                {extras_owned
                    .into_iter()
                    .map(|(k, v)| view! { <input type="hidden" name=k value=v/> }.into_any())
                    .collect_view()}
                // Changing page size always restarts at page 1 for this list.
                <input type="hidden" name=page_param_owned value="1"/>
                <label>
                    "Per page"
                    <select name="per_page" onchange="this.form.submit()">
                        {PAGE_SIZE_OPTIONS
                            .iter()
                            .copied()
                            .map(|n| {
                                let value = n.to_string();
                                let label = n.to_string();
                                let selected = n == page_size_u;
                                view! {
                                    <option value=value selected=selected>{label}</option>
                                }
                                .into_any()
                            })
                            .collect_view()}
                    </select>
                </label>
            </form>
        </div>
    }
    .into_any()
}

/// Slice `items` for the current page and render a pager underneath.
pub fn paginate_items<T: Clone>(items: Vec<T>, page: u32, page_size: usize) -> PageView<T> {
    crate::format::paginate_preserve_order(items, page, page_size)
}
