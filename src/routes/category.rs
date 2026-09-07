use actix_web::{web, HttpResponse};
use serde::Serialize;
use tracing::error;

use crate::state::AppState;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.route("/api/category/list", web::get().to(list_categories));
}

#[derive(Serialize)]
struct RenditionEntry {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bitrate: Option<i32>,
    kind: String,
    #[serde(rename = "mimeType")]
    mime_type: String,
}

#[derive(Serialize)]
struct CategoryEntry {
    name: String,
    renditions: Vec<RenditionEntry>,
    /// The order the playback fallback walks for this category, so a client can see
    /// what it will be given when the rendition it wants is not ready yet.
    #[serde(rename = "fallbackOrder")]
    fallback_order: Vec<String>,
}

async fn list_categories(state: web::Data<AppState>) -> HttpResponse {
    match build_category_list(&state) {
        Ok(cats) => HttpResponse::Ok().json(cats),
        Err(e) => {
            error!("Error building category list: {}", e);
            HttpResponse::InternalServerError().finish()
        }
    }
}

fn build_category_list(state: &AppState) -> anyhow::Result<Vec<CategoryEntry>> {
    let cfg = &state.config.transcode;
    let mut categories: Vec<CategoryEntry> = Vec::new();

    if !cfg.rendition_keys.is_empty() {
        let renditions = collect(&cfg.rendition_keys, state);
        if !renditions.is_empty() {
            categories.push(CategoryEntry {
                name: "default".to_string(),
                renditions,
                fallback_order: cfg.fallback_order_for_category(""),
            });
        }
    }

    for (name, keys) in &cfg.rendition_sets {
        if name.is_empty() || keys.is_empty() {
            continue;
        }
        let renditions = collect(keys, state);
        if !renditions.is_empty() {
            categories.push(CategoryEntry {
                fallback_order: cfg.fallback_order_for_category(&name.to_lowercase()),
                name: name.clone(),
                renditions,
            });
        }
    }

    Ok(categories)
}

fn collect(keys_str: &str, state: &AppState) -> Vec<RenditionEntry> {
    keys_str
        .split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .filter_map(|key| {
            let rc = state.config.transcode.get_rendition(key)?;
            Some(RenditionEntry {
                name: key.to_string(),
                width: rc.width,
                height: rc.height,
                bitrate: rc.bitrate,
                kind: rc.kind.clone(),
                mime_type: rc.mime(),
            })
        })
        .collect()
}
