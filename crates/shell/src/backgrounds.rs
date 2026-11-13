//! CSS background images: which URLs the page's displayed elements use, and fetching them.
//!
//! They are found after the cascade (like web fonts), only for elements that are displayed,
//! and kept in the page's `ImageStore` by URL, inside the budget its `<img>` files share. A
//! sleeping tab stores them with its other images, so it wakes without the network.

use css::{BackgroundLayer, Display, StyleMap};
use html::{Dom, NodeId};

use crate::images::{ImageStore, MAX_IMAGES};
use crate::pool;

const FETCH_WORKERS: usize = 6;

/// Where background files come from.
#[derive(Clone, Copy)]
pub enum BackgroundFrom<'a> {
    Network(Option<&'a net::Cookies>),
    /// A waking page: its store holds them already.
    Stored,
    /// Pages Broxee writes: only `data:` URLs.
    Inline,
}

/// The background files the page needs and does not have, as (URL as the style holds it,
/// bytes or `None`).
pub fn load(
    dom: &Dom,
    styles: &StyleMap,
    page_url: &net::Url,
    from: BackgroundFrom,
    store: &ImageStore,
) -> Vec<(String, Option<Vec<u8>>)> {
    if matches!(from, BackgroundFrom::Stored) {
        return Vec::new();
    }
    let room = MAX_IMAGES.saturating_sub(store.len());
    let wanted: Vec<String> = wanted(dom, styles)
        .into_iter()
        .filter(|u| !store.has_background(u))
        .take(room)
        .collect();
    if wanted.is_empty() {
        return Vec::new();
    }
    let context = net::Context::subresource(page_url);
    let jar = match from {
        BackgroundFrom::Network(cookies) => cookies.map(|c| (c, &context)),
        _ => None,
    };
    let network = matches!(from, BackgroundFrom::Network(_));
    let bytes = pool::map_ordered(wanted.clone(), FETCH_WORKERS, |url| {
        if url.starts_with("data:") {
            return net::decode_data_url(&url);
        }
        if !network {
            return None;
        }
        // A `style` attribute's URLs are relative to the page.
        let absolute = net::Url::parse(&url)
            .or_else(|_| page_url.join(&url))
            .ok()?;
        let resp = net::fetch_in(&absolute, jar).ok()?;
        (resp.status == 200).then_some(resp.body)
    });
    wanted.into_iter().zip(bytes).collect()
}

/// Background URLs of displayed elements, in document order, each once.
fn wanted(dom: &Dom, styles: &StyleMap) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in 0..dom.len() as NodeId {
        if dom.element(id).is_none() {
            continue;
        }
        let style = styles.style(id);
        if style.background_image == 0 {
            continue;
        }
        let layers = styles.backgrounds(style);
        if !layers
            .iter()
            .any(|l| matches!(l, BackgroundLayer::Image(_)))
            || hidden(dom, styles, id)
        {
            continue;
        }
        for layer in layers {
            if let BackgroundLayer::Image(img) = layer {
                if !out.iter().any(|u| **u == *img.url) {
                    out.push(img.url.to_string());
                }
            }
        }
    }
    out
}

/// Whether the element or an ancestor is not displayed.
fn hidden(dom: &Dom, styles: &StyleMap, id: NodeId) -> bool {
    let mut current = Some(id);
    while let Some(n) = current {
        if dom.element(n).is_some() && styles.style(n).display == Display::None {
            return true;
        }
        current = dom.parent(n);
    }
    false
}
